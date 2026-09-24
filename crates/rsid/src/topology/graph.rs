//! Pure scheduling model for the durable topology executor (#634, plan §2.3).
//!
//! Everything here is a function of the immutable definition snapshot plus
//! durable attempt/decision rows, so `advance` computes the same next step
//! after any restart. Graph parsing reuses the legacy runner's pure helpers.

use std::collections::{HashMap, HashSet};

use rsi_common::types::{FailurePolicy, UntilCondition};
use rsi_graph::format::{EdgeDef, WorkflowDefinition};

use crate::error::{DaemonError, Result};
use crate::session::graph_runner::{
    parse_failure_policies_from_metadata, parse_loop_edges_from_metadata,
    parse_scc_regions_from_metadata, parse_until_condition_from_metadata, topological_layers,
};
use crate::session::topology_ops::MAX_ITERATIONS;
use crate::topology::steps::WorkflowSteps;

/// Per node instance cap on charged attempts (plan §2.5).
pub(crate) const MAX_ATTEMPTS_PER_NODE: u32 = 3;
/// Per execution cap on all attempts, charged or not (plan §2.5).
pub(crate) const MAX_NODE_ATTEMPTS_PER_EXECUTION: u32 = 64;

/// Durable state of one `(node, iteration)` instance, derived from its latest
/// attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstanceState {
    /// No attempt has been reserved.
    Pending,
    /// The latest attempt is reserved, launching or running.
    InFlight,
    /// Succeeded, or resolved `accepted`.
    Complete,
    /// Terminal failure under `FailurePolicy::Skip`.
    Skipped,
    /// Terminal failure without a remaining retry.
    Failed,
    /// Waiting for a preserved-work resolution.
    Blocked,
    /// Every incoming edge was untaken (plan §3 dead-path skipping).
    Dead,
    /// Terminal failure handled by an outgoing `failure` or `completed` edge.
    Routed,
}

impl InstanceState {
    const fn is_done(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Skipped | Self::Dead | Self::Routed
        )
    }
}

/// Iteration progress of one loop region, derived from recorded decisions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RegionProgress {
    /// Iteration currently running (number of recorded `continue`s).
    pub(crate) current: u32,
    /// A `halt` has been recorded; `current` is the final iteration.
    pub(crate) halted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RegionDecision {
    Continue,
    Halt(&'static str),
}

/// The executable shape of one workflow snapshot.
#[derive(Debug)]
pub(crate) struct GraphShape {
    /// Node ids in topological order over forward edges.
    order: Vec<String>,
    forward_preds: HashMap<String, Vec<String>>,
    forward_edges: HashMap<String, Vec<EdgeDef>>,
    region_of: HashMap<String, usize>,
    regions: Vec<Vec<String>>,
    /// Per-region iteration cap from node `max_iterations` (min over the
    /// region, bounded by `MAX_ITERATIONS`); `None` when no node declares one.
    region_caps: Vec<Option<u32>>,
    retry_budgets: HashMap<String, u32>,
    sources: HashSet<String>,
    sinks: Vec<String>,
    until: Option<UntilCondition>,
    failure_policies: HashMap<String, FailurePolicy>,
    steps: WorkflowSteps,
}

impl GraphShape {
    pub(crate) fn from_workflow(workflow: &WorkflowDefinition) -> Result<Self> {
        let loop_edges = parse_loop_edges_from_metadata(&workflow.metadata);
        let regions = parse_scc_regions_from_metadata(&workflow.metadata);
        let layers = topological_layers(&workflow.nodes, &workflow.edges, &loop_edges)
            .map_err(DaemonError::InvalidParam)?;
        let loop_set: HashSet<(&str, &str)> = loop_edges
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
            .collect();
        let mut forward_preds: HashMap<String, Vec<String>> = HashMap::new();
        let mut forward_edges: HashMap<String, Vec<EdgeDef>> = HashMap::new();
        let mut has_incoming = HashSet::new();
        let mut has_outgoing = HashSet::new();
        for edge in &workflow.edges {
            has_incoming.insert(edge.target.as_str());
            has_outgoing.insert(edge.source.as_str());
            if loop_set.contains(&(edge.source.as_str(), edge.target.as_str())) {
                continue;
            }
            forward_preds
                .entry(edge.target.clone())
                .or_default()
                .push(edge.source.clone());
            forward_edges
                .entry(edge.target.clone())
                .or_default()
                .push(edge.clone());
        }
        let region_of: HashMap<String, usize> = regions
            .iter()
            .enumerate()
            .flat_map(|(index, region)| region.iter().map(move |node| (node.clone(), index)))
            .collect();
        let caps: HashMap<&str, u32> = workflow
            .nodes
            .iter()
            .filter_map(|node| {
                node.repeat_policy.as_ref().map(|policy| {
                    let cap = u32::try_from(policy.max_iterations).unwrap_or(MAX_ITERATIONS);
                    (node.id.as_str(), cap.max(1))
                })
            })
            .collect();
        let region_caps = regions
            .iter()
            .map(|region| {
                region
                    .iter()
                    .filter_map(|node| caps.get(node.as_str()).copied())
                    .min()
                    .map(|cap| cap.min(MAX_ITERATIONS))
            })
            .collect();
        let retry_budgets = workflow
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.clone(),
                    crate::session::graph_runner::failure_retry_budget(node),
                )
            })
            .collect();
        // Legacy runner semantics: a source has no incoming edge at all, a
        // sink no outgoing edge at all (loop edges included).
        let sources = workflow
            .nodes
            .iter()
            .filter(|node| !has_incoming.contains(node.id.as_str()))
            .map(|node| node.id.clone())
            .collect();
        let sinks = workflow
            .nodes
            .iter()
            .filter(|node| !has_outgoing.contains(node.id.as_str()))
            .map(|node| node.id.clone())
            .collect();
        Ok(Self {
            order: layers.into_iter().flatten().collect(),
            forward_preds,
            forward_edges,
            region_of,
            regions,
            region_caps,
            retry_budgets,
            sources,
            sinks,
            until: parse_until_condition_from_metadata(&workflow.metadata),
            failure_policies: parse_failure_policies_from_metadata(&workflow.metadata),
            steps: WorkflowSteps::from_workflow(workflow).map_err(DaemonError::InvalidParam)?,
        })
    }

    pub(crate) const fn region_count(&self) -> usize {
        self.regions.len()
    }

    pub(crate) fn region_nodes(&self, region: usize) -> &[String] {
        self.regions.get(region).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn region_of(&self, node: &str) -> Option<usize> {
        self.region_of.get(node).copied()
    }

    pub(crate) fn is_source(&self, node: &str) -> bool {
        self.sources.contains(node)
    }

    pub(crate) fn sinks(&self) -> &[String] {
        &self.sinks
    }

    pub(crate) fn forward_edges(&self, node: &str) -> &[EdgeDef] {
        self.forward_edges.get(node).map_or(&[], Vec::as_slice)
    }

    /// Typed steps and edge routing of the snapshot (#635).
    pub(crate) const fn steps(&self) -> &WorkflowSteps {
        &self.steps
    }

    pub(crate) fn failure_policy(&self, node: &str) -> Option<FailurePolicy> {
        self.failure_policies.get(node).copied()
    }

    pub(crate) const fn until(&self) -> Option<&UntilCondition> {
        self.until.as_ref()
    }

    /// `FailurePolicy::Retry` budget of a node. Legacy shapes keep the
    /// legacy runner's budget exactly. A typed topology follows plan §2.5:
    /// per-node `max_attempts` is 1 without `Retry`, otherwise the budget plus
    /// one, capped at `MAX_ATTEMPTS_PER_NODE`.
    pub(crate) fn retry_budget(&self, node: &str) -> u32 {
        let budget = self.retry_budgets.get(node).copied().unwrap_or(1);
        if self.steps.is_typed() {
            budget.min(MAX_ATTEMPTS_PER_NODE - 1)
        } else {
            budget
        }
    }

    /// Most iterations a node can run: 1 outside loops; inside a region the
    /// lesser of the topology `until` bound and the node cap (the legacy
    /// single iteration when neither exists), never above `MAX_ITERATIONS`.
    fn iteration_bound(&self, node: &str) -> u32 {
        let Some(region) = self.region_of(node) else {
            return 1;
        };
        let node_cap = self.region_caps.get(region).copied().flatten();
        match &self.until {
            None => node_cap.unwrap_or(1),
            Some(UntilCondition::MaxIterations(max)) => {
                (*max).max(1).min(node_cap.unwrap_or(MAX_ITERATIONS))
            }
            Some(_) => node_cap.unwrap_or(MAX_ITERATIONS),
        }
    }

    /// Charged-attempt cap of an execution: the plan §2.5 default, raised to
    /// the legacy runner's worst case (every node run once plus its full
    /// `FailurePolicy::Retry` budget, per iteration) so an ordinary workflow
    /// is never cut short relative to the kill-switch runner. Infrastructure
    /// losses are not charged against it (see `executor::retry_due`).
    ///
    /// A typed topology uses the plan §2.5 cap (`max_node_attempts`, default
    /// 64) unchanged; only legacy shapes need the parity derivation.
    pub(crate) fn attempt_cap(&self, stored: u32) -> u32 {
        if self.steps.is_typed() {
            return stored;
        }
        let legacy = self.order.iter().fold(0_u32, |total, node| {
            let per_iteration = self.retry_budget(node).saturating_add(1);
            total.saturating_add(per_iteration.saturating_mul(self.iteration_bound(node)))
        });
        stored.max(legacy)
    }

    /// The iteration of `node` a consumer should read: the same iteration
    /// inside one region, otherwise the source's final iteration.
    pub(crate) fn source_iteration(
        &self,
        source: &str,
        consumer_region: Option<usize>,
        consumer_iteration: u32,
        progress: &[RegionProgress],
    ) -> u32 {
        match self.region_of(source) {
            Some(region) if Some(region) == consumer_region => consumer_iteration,
            Some(region) => progress.get(region).map_or(0, |p| p.current),
            None => 0,
        }
    }

    /// The instance a node runs next: iteration 0 outside loops, the region's
    /// current iteration inside one; `None` once its region halted.
    fn next_iteration(&self, node: &str, progress: &[RegionProgress]) -> Option<u32> {
        self.region_of(node).map_or(Some(0), |region| {
            let region = progress.get(region).copied().unwrap_or_default();
            (!region.halted).then_some(region.current)
        })
    }

    fn predecessor_done(
        &self,
        pred: &str,
        consumer_region: Option<usize>,
        iteration: u32,
        state: &dyn Fn(&str, u32) -> InstanceState,
        progress: &[RegionProgress],
    ) -> bool {
        match self.region_of(pred) {
            Some(region) if Some(region) == consumer_region => state(pred, iteration).is_done(),
            Some(region) => {
                let region = progress.get(region).copied().unwrap_or_default();
                region.halted && state(pred, region.current).is_done()
            }
            None => state(pred, 0).is_done(),
        }
    }

    /// Instances with no attempt whose forward inputs are all done, with
    /// whether the instance is live: a source, or at least one incoming
    /// forward edge is taken. A non-live instance is dead-path skipped.
    /// `taken(edge, source_iteration)` evaluates one edge.
    pub(crate) fn ready_instances(
        &self,
        state: &dyn Fn(&str, u32) -> InstanceState,
        progress: &[RegionProgress],
        taken: &dyn Fn(&EdgeDef, u32) -> bool,
    ) -> Vec<(String, u32, bool)> {
        let mut ready = Vec::new();
        for node in &self.order {
            let Some(iteration) = self.next_iteration(node, progress) else {
                continue;
            };
            if state(node, iteration) != InstanceState::Pending {
                continue;
            }
            let region = self.region_of(node);
            let inputs_done = self.forward_preds.get(node).is_none_or(|preds| {
                preds
                    .iter()
                    .all(|pred| self.predecessor_done(pred, region, iteration, state, progress))
            });
            if inputs_done {
                let edges = self.forward_edges(node);
                let live = edges.is_empty()
                    || edges.iter().any(|edge| {
                        let at = self.source_iteration(&edge.source, region, iteration, progress);
                        taken(edge, at)
                    });
                ready.push((node.clone(), iteration, live));
            }
        }
        ready
    }

    /// Regions whose current iteration finished and still need a decision.
    pub(crate) fn regions_awaiting_decision(
        &self,
        state: &dyn Fn(&str, u32) -> InstanceState,
        progress: &[RegionProgress],
    ) -> Vec<(usize, u32)> {
        (0..self.regions.len())
            .filter_map(|region| {
                let current = progress.get(region).copied().unwrap_or_default();
                let complete = !current.halted
                    && self.regions[region]
                        .iter()
                        .all(|node| state(node, current.current).is_done());
                complete.then_some((region, current.current))
            })
            .collect()
    }

    /// Decide whether a finished region iteration continues. `lead_halted`
    /// is a `/halt` directive in one of the iteration's node results;
    /// `predicate_met` is the evaluated `index_exhausted` predicate.
    pub(crate) fn decide_region(
        &self,
        region: usize,
        completed_iteration: u32,
        lead_halted: bool,
        predicate_met: bool,
    ) -> RegionDecision {
        let completed = completed_iteration.saturating_add(1);
        if lead_halted {
            return RegionDecision::Halt("lead_halt");
        }
        let node_cap = self.region_caps.get(region).copied().flatten();
        match &self.until {
            // No topology `until`: node `max_iterations` guards the loop; with
            // neither, the legacy single-iteration default applies.
            None if completed >= node_cap.unwrap_or(1) => {
                return RegionDecision::Halt("iteration_cap");
            }
            Some(UntilCondition::MaxIterations(max)) if completed >= *max => {
                return RegionDecision::Halt("max_iterations");
            }
            Some(UntilCondition::Predicate(expr)) if expr != "index_exhausted" => {
                return RegionDecision::Halt("no_until_guard");
            }
            Some(UntilCondition::Predicate(_)) if predicate_met => {
                return RegionDecision::Halt("predicate");
            }
            _ => {}
        }
        if completed >= node_cap.unwrap_or(MAX_ITERATIONS) {
            return RegionDecision::Halt("iteration_cap");
        }
        RegionDecision::Continue
    }

    /// Every node's final instance is done.
    pub(crate) fn is_complete(
        &self,
        state: &dyn Fn(&str, u32) -> InstanceState,
        progress: &[RegionProgress],
    ) -> bool {
        self.order.iter().all(|node| {
            self.region_of(node).map_or_else(
                || state(node, 0).is_done(),
                |region| {
                    let region = progress.get(region).copied().unwrap_or_default();
                    region.halted && state(node, region.current).is_done()
                },
            )
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rsi_graph::data::Value as GraphValue;
    use rsi_graph::format::{NodeDef, RepeatPolicy};

    fn looped(caps: &[(&str, usize)], until: Option<&UntilCondition>) -> WorkflowDefinition {
        let mut nodes: Vec<NodeDef> = ["A", "B", "C"]
            .iter()
            .map(|id| NodeDef::action(*id, *id))
            .collect();
        for (id, cap) in caps {
            let node = nodes.iter_mut().find(|node| node.id == *id).unwrap();
            node.repeat_policy = Some(RepeatPolicy {
                max_iterations: *cap,
                termination: None,
            });
        }
        let mut workflow = WorkflowDefinition {
            version: "1.0".into(),
            name: "loop".into(),
            description: String::new(),
            nodes,
            edges: vec![
                EdgeDef::new("A", "B"),
                EdgeDef::new("B", "C"),
                EdgeDef::new("C", "B"),
            ],
            metadata: std::collections::BTreeMap::default(),
        };
        workflow.metadata.insert(
            "loop_edges".into(),
            GraphValue::String(r#"[{"from":"C","to":"B"}]"#.into()),
        );
        workflow.metadata.insert(
            "scc_regions".into(),
            GraphValue::String(r#"[["B","C"]]"#.into()),
        );
        if let Some(until) = until {
            workflow.metadata.insert(
                "until_condition".into(),
                GraphValue::String(serde_json::to_string(until).unwrap()),
            );
        }
        workflow
    }

    /// T2-A6: loops stop at the per-node cap, at the topology `until`, and
    /// never beyond `MAX_ITERATIONS`, whichever comes first.
    #[test]
    fn t2_a6_loop_caps_bound_every_region() {
        let unbounded =
            GraphShape::from_workflow(&looped(&[], Some(&UntilCondition::LeadHalt))).unwrap();
        assert_eq!(
            unbounded.decide_region(0, MAX_ITERATIONS - 2, false, false),
            RegionDecision::Continue
        );
        assert_eq!(
            unbounded.decide_region(0, MAX_ITERATIONS - 1, false, false),
            RegionDecision::Halt("iteration_cap")
        );
        assert_eq!(
            unbounded.decide_region(0, 0, true, false),
            RegionDecision::Halt("lead_halt")
        );

        let capped = GraphShape::from_workflow(&looped(
            &[("C", 3)],
            Some(&UntilCondition::MaxIterations(40)),
        ))
        .unwrap();
        assert_eq!(
            capped.decide_region(0, 1, false, false),
            RegionDecision::Continue
        );
        assert_eq!(
            capped.decide_region(0, 2, false, false),
            RegionDecision::Halt("iteration_cap")
        );

        let until = GraphShape::from_workflow(&looped(
            &[("C", 5)],
            Some(&UntilCondition::MaxIterations(2)),
        ))
        .unwrap();
        assert_eq!(
            until.decide_region(0, 1, false, false),
            RegionDecision::Halt("max_iterations")
        );
        // No declared `until` and no node cap keeps the legacy default.
        let legacy = GraphShape::from_workflow(&looped(&[], None)).unwrap();
        assert_eq!(
            legacy.decide_region(0, 0, false, false),
            RegionDecision::Halt("iteration_cap")
        );
        // Round 1 (loop_cap_without_until): a cap-only loop runs to its cap.
        let cap_only = GraphShape::from_workflow(&looped(&[("B", 5), ("C", 7)], None)).unwrap();
        assert_eq!(
            cap_only.decide_region(0, 3, false, false),
            RegionDecision::Continue
        );
        assert_eq!(
            cap_only.decide_region(0, 4, false, false),
            RegionDecision::Halt("iteration_cap")
        );
    }

    #[test]
    fn readiness_follows_forward_edges_and_region_iterations() {
        let shape =
            GraphShape::from_workflow(&looped(&[], Some(&UntilCondition::MaxIterations(2))))
                .unwrap();
        let mut progress = vec![RegionProgress::default()];
        let none = |_: &str, _: u32| InstanceState::Pending;
        let taken = |_: &EdgeDef, _: u32| true;
        assert_eq!(
            shape.ready_instances(&none, &progress, &taken),
            vec![("A".into(), 0, true)]
        );

        let a_done = |node: &str, _: u32| {
            if node == "A" {
                InstanceState::Complete
            } else {
                InstanceState::Pending
            }
        };
        assert_eq!(
            shape.ready_instances(&a_done, &progress, &taken),
            vec![("B".into(), 0, true)]
        );

        let first_iteration = |node: &str, iteration: u32| {
            if node == "A" || iteration == 0 {
                InstanceState::Complete
            } else {
                InstanceState::Pending
            }
        };
        assert_eq!(
            shape.regions_awaiting_decision(&first_iteration, &progress),
            vec![(0, 0)]
        );
        progress[0].current = 1;
        assert_eq!(
            shape.ready_instances(&first_iteration, &progress, &taken),
            vec![("B".into(), 1, true)]
        );
        assert!(!shape.is_complete(&first_iteration, &progress));
        let all = |_: &str, _: u32| InstanceState::Complete;
        progress[0].halted = true;
        assert!(shape.is_complete(&all, &progress));
    }
}
