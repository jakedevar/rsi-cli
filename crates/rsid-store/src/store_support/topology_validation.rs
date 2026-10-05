//! Pure topology-definition validators, moved down from
//! `session::topology_ops` and `topology::steps` so `store` has no edge into
//! them (issue #1021 S3a). Both originals re-export these at their old paths.

use crate::error::DaemonError;
use rsi_common::types::{
    EdgeWhen, SessionKind, TOPOLOGY_FORBIDDEN_AUTHOR_PARAMS, TopologyDefinition, TopologyStep,
    legal_children,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Daemon-enforced hard cap on per-node iteration counts. Per locked
/// decision §B1 (project INDEX). Any node with `max_iterations > 32`
/// is rejected at validation time; loops without a termination guard
/// are also rejected.
pub const MAX_ITERATIONS: u32 = 32;

/// Topology-domain validation and operational errors. Internal to the
/// daemon; mapped to `DaemonError::InvalidParam` for the RPC surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyError {
    /// A topology with the same `name` already exists.
    DuplicateName,
    /// An edge references a node id that does not appear in `def.nodes`.
    UnknownNode(String),
    /// The acyclic-edge subset (loop_edge: false) contains a cycle.
    CycleDetected,
    /// A node declares a kind not in `legal_children(Some(Epic))`.
    IllegalNodeKind(SessionKind),
    /// A node declares `max_iterations` above `MAX_ITERATIONS`.
    IterationCapExceeded(u32),
    /// The topology has a loop region with no termination guard
    /// (no `def.until`, no per-node `max_iterations` within the SCC).
    UnboundedLoop,
    /// Topology id not found in the table.
    NotFound,
    /// Delete rejected because the topology is still referenced by an
    /// Epic's `workflow_id`. Carries the offending Epic UUID.
    InUse(Uuid),
    /// A typed step, edge routing, or author parameter is invalid (#635).
    InvalidStep(String),
}

impl From<TopologyError> for DaemonError {
    fn from(err: TopologyError) -> Self {
        match err {
            TopologyError::DuplicateName => {
                DaemonError::InvalidParam("topology name already exists".to_string())
            }
            TopologyError::UnknownNode(id) => {
                DaemonError::InvalidParam(format!("edge references unknown node id: {}", id))
            }
            TopologyError::CycleDetected => DaemonError::InvalidParam(
                "topology DAG contains a cycle (excluding loop_edge: true)".to_string(),
            ),
            TopologyError::IllegalNodeKind(kind) => DaemonError::InvalidParam(format!(
                "illegal node kind: {:?} (must be in legal_children(Some(Epic)))",
                kind
            )),
            TopologyError::IterationCapExceeded(n) => DaemonError::InvalidParam(format!(
                "max_iterations {} exceeds hard cap {}",
                n, MAX_ITERATIONS
            )),
            TopologyError::UnboundedLoop => DaemonError::InvalidParam(
                "topology contains a loop region with no termination guard \
                 (define until or per-node max_iterations)"
                    .to_string(),
            ),
            TopologyError::NotFound => DaemonError::InvalidParam("topology not found".to_string()),
            TopologyError::InUse(epic_id) => DaemonError::InvalidParam(format!(
                "topology in use by Epic {}: clear the reference before deleting",
                epic_id
            )),
            TopologyError::InvalidStep(message) => Self::InvalidParam(message),
        }
    }
}

// ─── Pure-fn validators ─────────────────────────────────────────────────

/// Run all validators in sequence. Returns the first failure.
///
/// `params` is otherwise opaque to the daemon, except for the typed keys
/// `step` and `when` and the refused author keys (`argv`, `env`, `script`,
/// `script_path`, `effect_class`), which `topology::steps` validates (#635).
pub fn validate_topology_definition(
    def: &TopologyDefinition,
) -> std::result::Result<(), TopologyError> {
    validate_node_kinds(def)?;
    validate_edges_reference_existing_nodes(def)?;
    detect_cycles(def)?;
    validate_iteration_caps(def)?;
    validate_topology(def).map_err(TopologyError::InvalidStep)?;
    Ok(())
}

/// Every node.kind must be in `legal_children(Some(SessionKind::Epic))`.
/// Canonical source: `crates/rsi-common/src/types.rs:740-751`.
pub(crate) fn validate_node_kinds(
    def: &TopologyDefinition,
) -> std::result::Result<(), TopologyError> {
    let legal = legal_children(Some(SessionKind::Epic));
    for node in &def.nodes {
        if !legal.contains(&node.kind) {
            return Err(TopologyError::IllegalNodeKind(node.kind));
        }
    }
    Ok(())
}

/// Every edge.from / edge.to must resolve to a node.id in def.nodes.
pub(crate) fn validate_edges_reference_existing_nodes(
    def: &TopologyDefinition,
) -> std::result::Result<(), TopologyError> {
    let known: HashSet<&str> = def.nodes.iter().map(|n| n.id.as_str()).collect();
    for edge in &def.edges {
        if !known.contains(edge.from.as_str()) {
            return Err(TopologyError::UnknownNode(edge.from.clone()));
        }
        if !known.contains(edge.to.as_str()) {
            return Err(TopologyError::UnknownNode(edge.to.clone()));
        }
    }
    Ok(())
}

/// Kahn's algorithm over the edge subset where loop_edge == false.
/// Returns CycleDetected if any node remains with non-zero in-degree.
pub(crate) fn detect_cycles(def: &TopologyDefinition) -> std::result::Result<(), TopologyError> {
    let mut in_degree: HashMap<&str, usize> =
        def.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = def
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), Vec::new()))
        .collect();

    for edge in def.edges.iter().filter(|e| !e.loop_edge) {
        adj.entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
        *in_degree.entry(edge.to.as_str()).or_insert(0) += 1;
    }

    // Queue of zero-in-degree nodes.
    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter_map(|(n, d)| if *d == 0 { Some(*n) } else { None })
        .collect();

    let mut visited = 0usize;
    while let Some(node) = queue.pop() {
        visited += 1;
        if let Some(neighbors) = adj.get(node) {
            for &nb in neighbors {
                if let Some(d) = in_degree.get_mut(nb) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        queue.push(nb);
                    }
                }
            }
        }
    }

    if visited != def.nodes.len() {
        return Err(TopologyError::CycleDetected);
    }
    Ok(())
}

/// (1) Every node.max_iterations <= MAX_ITERATIONS.
/// (2) If any loop region exists (SCC of size > 1 or a self-loop in the
///     full edge set), it must declare at least one termination guard:
///     def.until.is_some() OR any node in the SCC has max_iterations.is_some().
pub(crate) fn validate_iteration_caps(
    def: &TopologyDefinition,
) -> std::result::Result<(), TopologyError> {
    for node in &def.nodes {
        if let Some(cap) = node.max_iterations {
            if cap > MAX_ITERATIONS {
                return Err(TopologyError::IterationCapExceeded(cap));
            }
        }
    }

    // Loop region check — SCCs over the full edge set (including loop_edge).
    let sccs = strongly_connected_components(def);

    // Helper: a single-node SCC is a loop region only if there's a self-loop.
    let is_loop_region = |scc: &Vec<String>| -> bool {
        if scc.len() > 1 {
            return true;
        }
        if scc.len() == 1 {
            return def.edges.iter().any(|e| e.from == scc[0] && e.to == scc[0]);
        }
        false
    };

    let has_loop_region = sccs.iter().any(is_loop_region);

    if has_loop_region && def.until.is_none() {
        // At least one node in some loop SCC must declare max_iterations.
        let any_loop_node_capped = sccs.iter().any(|scc| {
            if !is_loop_region(scc) {
                return false;
            }
            scc.iter().any(|id| {
                def.nodes
                    .iter()
                    .any(|n| n.id == *id && n.max_iterations.is_some())
            })
        });

        if !any_loop_node_capped {
            return Err(TopologyError::UnboundedLoop);
        }
    }

    Ok(())
}

/// Wrapper for graph_runner: returns SCC regions sorted by minimum node-id for determinism.
///
/// Filters out trivial SCCs (single node with no self-loop). A loop region has ≥2 members
/// OR a single node with a self-loop edge (`loop_edge: true`, from == to).
/// Each SCC is sorted internally by node-id; SCCs are sorted by their minimum node-id.
pub fn compute_scc_regions(def: &TopologyDefinition) -> Vec<Vec<String>> {
    let mut sccs = strongly_connected_components(def);
    // Filter out trivial SCCs (single node, no self-loop).
    let loop_node_ids: std::collections::HashSet<&str> = def
        .edges
        .iter()
        .filter(|e| e.loop_edge && e.from == e.to)
        .map(|e| e.from.as_str())
        .collect();
    sccs.retain(|scc| scc.len() > 1 || loop_node_ids.contains(scc[0].as_str()));
    // Sort each SCC internally by node-id for stable ordering.
    for scc in &mut sccs {
        scc.sort();
    }
    // Sort SCCs by their minimum node-id.
    sccs.sort_by(|a, b| a[0].cmp(&b[0]));
    sccs
}

/// Tarjan's algorithm to find strongly connected components over the
/// full edge set (including loop_edge: true). Returns Vec<Vec<node_id>>
/// where each inner Vec is one SCC. Used by `validate_iteration_caps`
/// to identify loop regions.
pub(crate) fn strongly_connected_components(def: &TopologyDefinition) -> Vec<Vec<String>> {
    // Build adjacency list keyed by usize indices into def.nodes.
    let node_ids: Vec<&str> = def.nodes.iter().map(|n| n.id.as_str()).collect();
    let index_of: HashMap<&str, usize> = node_ids
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); node_ids.len()];
    for edge in &def.edges {
        if let (Some(&u), Some(&v)) = (
            index_of.get(edge.from.as_str()),
            index_of.get(edge.to.as_str()),
        ) {
            adj[u].push(v);
        }
    }

    // Tarjan state.
    let mut index = 0i64;
    let mut stack: Vec<usize> = Vec::new();
    let mut on_stack = vec![false; node_ids.len()];
    let mut indices: Vec<i64> = vec![-1; node_ids.len()];
    let mut lowlink: Vec<i64> = vec![-1; node_ids.len()];
    let mut result: Vec<Vec<String>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn strongconnect(
        v: usize,
        index: &mut i64,
        stack: &mut Vec<usize>,
        on_stack: &mut [bool],
        indices: &mut [i64],
        lowlink: &mut [i64],
        adj: &[Vec<usize>],
        node_ids: &[&str],
        result: &mut Vec<Vec<String>>,
    ) {
        indices[v] = *index;
        lowlink[v] = *index;
        *index += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in &adj[v] {
            if indices[w] == -1 {
                strongconnect(
                    w, index, stack, on_stack, indices, lowlink, adj, node_ids, result,
                );
                lowlink[v] = lowlink[v].min(lowlink[w]);
            } else if on_stack[w] {
                lowlink[v] = lowlink[v].min(indices[w]);
            }
        }

        if lowlink[v] == indices[v] {
            let mut scc = Vec::new();
            while let Some(w) = stack.pop() {
                on_stack[w] = false;
                scc.push(node_ids[w].to_string());
                if w == v {
                    break;
                }
            }
            result.push(scc);
        }
    }

    for v in 0..node_ids.len() {
        if indices[v] == -1 {
            strongconnect(
                v,
                &mut index,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlink,
                &adj,
                &node_ids,
                &mut result,
            );
        }
    }

    result
}

/// One node of the normalized validation graph.
pub struct StepNode<'a> {
    pub id: &'a str,
    pub step: Option<&'a TopologyStep>,
    /// Author parameter keys (or `key=value` tags) on the node.
    pub author_keys: Vec<&'a str>,
}

/// One edge of the normalized validation graph.
pub struct StepEdge<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub loop_edge: bool,
    pub when: EdgeWhen,
}

/// Plan §3 static rules 1, 2, 7, 8 and 9 plus edge-routing consistency.
pub fn validate_graph(nodes: &[StepNode<'_>], edges: &[StepEdge<'_>]) -> Result<(), String> {
    let known: HashSet<&str> = nodes.iter().map(|node| node.id).collect();
    for node in nodes {
        // Rule 8: never an author-chosen argv, env, script or effect class.
        if let Some(key) = node
            .author_keys
            .iter()
            .find(|key| TOPOLOGY_FORBIDDEN_AUTHOR_PARAMS.contains(key))
        {
            return Err(format!(
                "node {}: author-supplied {key} is refused; command nodes run only daemon catalog ops (#645)",
                node.id
            ));
        }
    }
    let mut forward_preds: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        if !known.contains(edge.from) || !known.contains(edge.to) {
            return Err(format!(
                "edge {} -> {} names an unknown node",
                edge.from, edge.to
            ));
        }
        let source_step = nodes
            .iter()
            .find(|node| node.id == edge.from)
            .and_then(|node| node.step);
        match edge.when {
            EdgeWhen::Success | EdgeWhen::Failure | EdgeWhen::Completed => {}
            EdgeWhen::GateTrue | EdgeWhen::GateFalse => {
                if !matches!(source_step, Some(TopologyStep::Gate { .. })) {
                    return Err(format!(
                        "edge {} -> {}: gate_true/gate_false edges must leave a gate node",
                        edge.from, edge.to
                    ));
                }
            }
            // Rule 9: review/land are refused until the T3b prerequisites.
            EdgeWhen::VerdictAccepted | EdgeWhen::VerdictChangesRequested => {
                return Err(format!(
                    "edge {} -> {}: verdict routing needs review nodes, which are not available yet",
                    edge.from, edge.to
                ));
            }
        }
        if edge.loop_edge && edge.when != EdgeWhen::Success {
            return Err(format!(
                "loop edge {} -> {} must be a success edge",
                edge.from, edge.to
            ));
        }
        if !edge.loop_edge {
            forward_preds.entry(edge.to).or_default().push(edge.from);
        }
    }
    for node in nodes {
        match node.step {
            None | Some(TopologyStep::Session { .. }) => {}
            // Rule 7: the op is in the catalog and its params validate.
            Some(TopologyStep::Command { op }) => op
                .validate()
                .map_err(|error| format!("node {}: {error}", node.id))?,
            // Rule 2: gate paths reference ancestors only.
            Some(TopologyStep::Gate { condition }) => {
                let referenced = condition
                    .validate()
                    .map_err(|error| format!("node {}: {error}", node.id))?;
                let ancestors = ancestors(node.id, &forward_preds);
                if let Some(missing) = referenced
                    .iter()
                    .find(|id| !ancestors.contains(id.as_str()))
                {
                    return Err(format!(
                        "node {}: gate path references {missing}, which is not an ancestor",
                        node.id
                    ));
                }
            }
        }
    }
    Ok(())
}

fn ancestors<'a>(node: &str, forward_preds: &HashMap<&str, Vec<&'a str>>) -> HashSet<&'a str> {
    let mut seen = HashSet::new();
    let mut stack: Vec<&str> = forward_preds.get(node).cloned().unwrap_or_default();
    while let Some(next) = stack.pop() {
        if seen.insert(next)
            && let Some(preds) = forward_preds.get(next)
        {
            stack.extend(preds.iter().copied());
        }
    }
    seen
}

/// Upsert-time validation over a stored topology definition.
pub fn validate_topology(def: &TopologyDefinition) -> Result<(), String> {
    let mut steps = Vec::with_capacity(def.nodes.len());
    let mut routing: HashMap<(String, String), EdgeWhen> = HashMap::new();
    for node in &def.nodes {
        steps.push(node.step()?);
        for (from, when) in node.incoming_when()? {
            if !def
                .edges
                .iter()
                .any(|edge| edge.from == from && edge.to == node.id)
            {
                return Err(format!(
                    "node {}: when names {from}, which has no edge into it",
                    node.id
                ));
            }
            routing.insert((from, node.id.clone()), when);
        }
    }
    let nodes: Vec<StepNode<'_>> = def
        .nodes
        .iter()
        .zip(&steps)
        .map(|(node, step)| StepNode {
            id: &node.id,
            step: step.as_ref(),
            author_keys: node.params.keys().map(String::as_str).collect(),
        })
        .collect();
    let edges: Vec<StepEdge<'_>> = def
        .edges
        .iter()
        .map(|edge| StepEdge {
            from: &edge.from,
            to: &edge.to,
            loop_edge: edge.loop_edge,
            when: routing
                .get(&(edge.from.clone(), edge.to.clone()))
                .copied()
                .unwrap_or_default(),
        })
        .collect();
    validate_graph(&nodes, &edges)
}

/// Stable namespace for v5-derived workflow ids that mirror topologies.
///
/// **MUST NEVER CHANGE across releases** — the v5 algebra depends on a fixed
/// namespace UUID. Bumping it would break idempotency for every already-
/// mirrored topology (new id ≠ old id), creating orphan duplicates.
pub(crate) const BRIDGE_NAMESPACE_UUID: Uuid = uuid::uuid!("c5c5f8d6-3d6a-4f5e-9b4e-1f8c3a7d6b9e");

/// Derive the deterministic workflow row id for a given topology.
///
/// The bridge mirrors each topology into the `workflows` table for `gv`-picker
/// visibility. The mirrored row's primary key is `Uuid::new_v5(NS, topology_id)`
/// so the upsert path is idempotent via `ON CONFLICT(id) DO UPDATE`.
pub fn derive_workflow_id(topology_id: Uuid) -> Uuid {
    Uuid::new_v5(&BRIDGE_NAMESPACE_UUID, topology_id.as_bytes())
}
