//! DB-stored named topology template CRUD on `SessionManager`.
//!
//! Mirrors `crates/rsid/src/session/hierarchy_ops.rs` structurally:
//!   - `impl SessionManager` block with async public methods.
//!   - Free-standing pure-fn validators (`pub(crate)` so unit tests reach them).
//!   - `TopologyError` enum + `From<TopologyError> for DaemonError`.
//!
//! Validation pipeline (pure, runs before any DB lock):
//!   1. `validate_node_kinds` — every node.kind in
//!      `legal_children(Some(SessionKind::Epic))`.
//!   2. `validate_edges_reference_existing_nodes` — every edge endpoint
//!      resolves to a node id.
//!   3. `detect_cycles` — Kahn's algorithm over edges with `loop_edge: false`.
//!   4. `validate_iteration_caps` — per-node `max_iterations <= MAX_ITERATIONS`,
//!      and every loop SCC declares at least one termination guard.

use super::SessionManager;
use crate::error::{DaemonError, Result};
use rsi_common::types::{SessionKind, Topology, TopologyDefinition, legal_children};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Daemon-enforced hard cap on per-node iteration counts. Per locked
/// decision §B1 (project INDEX). Any node with `max_iterations > 32`
/// is rejected at validation time; loops without a termination guard
/// are also rejected.
pub(crate) const MAX_ITERATIONS: u32 = 32;

/// Topology-domain validation and operational errors. Internal to the
/// daemon; mapped to `DaemonError::InvalidParam` for the RPC surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TopologyError {
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

impl SessionManager {
    /// List topologies, optionally filtered by name prefix.
    pub async fn list_topologies(&self, name_prefix: Option<String>) -> Result<Vec<Topology>> {
        let store = self.store.clone();
        let prefix = name_prefix;
        let result = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_topologies(prefix.as_deref())
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        Ok(result)
    }

    /// Create a new named topology after validating the definition. Returns
    /// the new row's UUID. Name uniqueness is enforced both daemon-side
    /// (before INSERT) and at the DB layer (UNIQUE constraint).
    pub async fn create_topology(
        &self,
        name: String,
        definition: TopologyDefinition,
    ) -> Result<Uuid> {
        validate_topology_definition(&definition)?;

        let now = chrono::Utc::now();
        let topology = Topology {
            id: Uuid::new_v4(),
            name: name.clone(),
            definition,
            created_at: now,
            updated_at: now,
        };

        // Daemon-side name-uniqueness check. The persistence handle is the
        // fire-and-forget write path, so we cannot share a single critical
        // section with the INSERT. Returning a clean InvalidParam here is
        // the steady-state path; the DB UNIQUE constraint remains the
        // final guard against a race.
        let store = self.store.clone();
        let name_check = name.clone();
        let conflict = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.topology_name_exists(&name_check, None)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        if conflict.is_some() {
            return Err(TopologyError::DuplicateName.into());
        }

        self.persistence.insert_topology(topology.clone()).await?;
        self.upsert_bridged_workflow(&topology, None).await?;
        Ok(topology.id)
    }

    /// Update an existing topology row. Either or both of `name` and
    /// `definition` may be `None` (no-op for that field). When both are
    /// `None`, the call is rejected with InvalidParam("nothing to update").
    pub async fn update_topology(
        &self,
        id: Uuid,
        name: Option<String>,
        definition: Option<TopologyDefinition>,
    ) -> Result<()> {
        if name.is_none() && definition.is_none() {
            return Err(DaemonError::InvalidParam(
                "update_topology: at least one of name or definition required".to_string(),
            ));
        }

        if let Some(ref def) = definition {
            validate_topology_definition(def)?;
        }

        // Fetch the existing row.
        let store = self.store.clone();
        let existing = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_topology(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??
        .ok_or_else(|| DaemonError::from(TopologyError::NotFound))?;

        // Name uniqueness (exclude self).
        if let Some(ref new_name) = name {
            let store = self.store.clone();
            let name_check = new_name.clone();
            let conflict = tokio::task::spawn_blocking(move || {
                let store = store.blocking_lock();
                store.topology_name_exists(&name_check, Some(id))
            })
            .await
            .map_err(|e| DaemonError::Store(e.to_string()))??;
            if conflict.is_some() {
                return Err(TopologyError::DuplicateName.into());
            }
        }

        let updated = Topology {
            id: existing.id,
            name: name.unwrap_or(existing.name),
            definition: definition.unwrap_or(existing.definition),
            created_at: existing.created_at,
            updated_at: chrono::Utc::now(),
        };

        self.persistence.update_topology(updated).await?;
        Ok(())
    }

    /// Delete a topology row. Rejects with `TopologyError::InUse` if any
    /// Epic session still references the topology via `workflow_id`.
    /// Per-session `workflow_id_override` references are NOT checked
    /// (Decision 1 in plan); those resolve to `None` at read time.
    pub async fn delete_topology(&self, id: Uuid) -> Result<()> {
        let store = self.store.clone();
        let referenced = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.is_topology_referenced(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        if let Some(epic_id) = referenced {
            return Err(TopologyError::InUse(epic_id).into());
        }

        // Verify the row exists (so we can return NotFound cleanly).
        let store = self.store.clone();
        let exists = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_topology(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        if exists.is_none() {
            return Err(TopologyError::NotFound.into());
        }

        self.persistence.delete_topology(id).await?;
        Ok(())
    }

    /// Fetch a single topology by id. Returns NotFound when no row matches.
    pub async fn get_topology(&self, id: Uuid) -> Result<Topology> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_topology(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        result.ok_or_else(|| TopologyError::NotFound.into())
    }
}

// ─── Pure-fn validators ─────────────────────────────────────────────────

/// Run all validators in sequence. Returns the first failure.
///
/// `params` is otherwise opaque to the daemon, except for the typed keys
/// `step` and `when` and the refused author keys (`argv`, `env`, `script`,
/// `script_path`, `effect_class`), which `topology::steps` validates (#635).
pub(crate) fn validate_topology_definition(
    def: &TopologyDefinition,
) -> std::result::Result<(), TopologyError> {
    validate_node_kinds(def)?;
    validate_edges_reference_existing_nodes(def)?;
    detect_cycles(def)?;
    validate_iteration_caps(def)?;
    crate::topology::steps::validate_topology(def).map_err(TopologyError::InvalidStep)?;
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
pub(crate) fn compute_scc_regions(def: &TopologyDefinition) -> Vec<Vec<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{TopologyEdge, TopologyNode, TopologyStep, UntilCondition};

    fn node(id: &str, kind: SessionKind) -> TopologyNode {
        TopologyNode {
            id: id.to_string(),
            kind,
            label: id.to_string(),
            prereqs: Vec::new(),
            max_iterations: None,
            on_failure: None,
            params: std::collections::HashMap::new(),
        }
    }

    fn edge(from: &str, to: &str, loop_edge: bool) -> TopologyEdge {
        TopologyEdge {
            from: from.to_string(),
            to: to.to_string(),
            loop_edge,
        }
    }

    #[allow(clippy::expect_used)]
    fn validate_stored_automation_fixture(json: &str) -> TopologyDefinition {
        let stored: rsi_common::types::Topology =
            serde_json::from_str(json).expect("stored topology decodes");
        let definition = stored.definition;

        // This is the same definition validator called by CreateTopology and
        // UpdateTopology, including T3a step, gate and catalog validation.
        validate_topology_definition(&definition).expect("stored topology validates at upsert");

        // The bridge's custody planner runs before an execution row is created.
        // Build its NodeDef/EdgeDef projection from the stored topology fields
        // consumed by that planner (custody.from and graph edges).
        let nodes = definition
            .nodes
            .iter()
            .map(|node| {
                let mut projected =
                    rsi_graph::format::NodeDef::action(node.id.clone(), node.label.clone());
                if let Some(from) = node
                    .params
                    .get("custody")
                    .and_then(|custody| custody.get("from"))
                    .and_then(serde_json::Value::as_str)
                {
                    projected.tags.push(format!("custody.from={from}"));
                }
                projected
            })
            .collect::<Vec<_>>();
        let edges = definition
            .edges
            .iter()
            .map(|edge| rsi_graph::format::EdgeDef::new(edge.from.clone(), edge.to.clone()))
            .collect::<Vec<_>>();
        let loop_edges = definition
            .edges
            .iter()
            .filter(|edge| edge.loop_edge)
            .map(|edge| (edge.from.clone(), edge.to.clone()))
            .collect::<Vec<_>>();
        crate::topology::custody::plan_topology_custody(&nodes, &edges, &loop_edges, &[])
            .expect("stored topology has a resolvable whole-workflow custody plan");

        // Operator CreateTopology does not have the T4 agent allowed_launches
        // policy context. Check the explicit static model fields available in
        // these operator-authored definitions; routed OpenRouter models may
        // have no family known to the current classifier.
        for node in &definition.nodes {
            if !matches!(
                node.step().expect("typed step decodes"),
                Some(TopologyStep::Session { .. })
            ) {
                continue;
            }
            let provider = match node
                .params
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .expect("session provider is explicit")
            {
                "codex" => rsi_common::types::SessionProvider::Codex,
                "openrouter" => rsi_common::types::SessionProvider::OpenRouter,
                provider => panic!("unexpected fixture provider {provider}"),
            };
            let model = node
                .params
                .get("model")
                .and_then(serde_json::Value::as_str)
                .filter(|model| !model.is_empty())
                .expect("session model is explicit");
            assert!(
                node.params
                    .get("effort")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|effort| !effort.is_empty()),
                "session effort is explicit"
            );
            let family = rsi_common::model_utils::vendor_family(provider, model);
            if provider == rsi_common::types::SessionProvider::Codex {
                assert_eq!(family, Some(rsi_common::model_utils::VendorFamily::OpenAi));
                assert!(
                    rsi_common::model_utils::known_effort_ladder(model)
                        .is_some_and(|levels| levels.contains(&"medium"))
                );
            }
        }

        definition
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn stored_topology_automation_definitions_validate() {
        let baseline = validate_stored_automation_fixture(include_str!(
            "../../../../docs/topology-on-epic/topologies/rolling-health-baseline.json"
        ));
        let baseline_kinds = baseline
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.as_str(),
                    node.step().expect("step").expect("typed step").kind_name(),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(baseline_kinds["baseline"], "command");
        assert_eq!(baseline_kinds["healthy"], "gate");
        assert_eq!(baseline_kinds["report"], "session");
        let baseline_gate = baseline
            .nodes
            .iter()
            .find(|node| node.id == "healthy")
            .expect("baseline gate node");
        assert_eq!(
            baseline_gate.params["when"]["baseline"],
            serde_json::Value::String("completed".into())
        );
        let baseline_session = baseline
            .nodes
            .iter()
            .find(|node| node.id == "report")
            .expect("baseline report node");
        assert_eq!(
            baseline_session.params["provider"],
            serde_json::Value::String("codex".into())
        );
        assert_eq!(
            baseline_session.params["model"],
            serde_json::Value::String("gpt-6-luna".into())
        );
        assert_eq!(
            baseline_session.params["effort"],
            serde_json::Value::String("medium".into())
        );
        assert_eq!(
            baseline_session.params["when"]["baseline"],
            serde_json::Value::String("completed".into())
        );
        assert_eq!(
            baseline_session.params["custody"]["from"],
            serde_json::Value::String("node:baseline".into())
        );

        let audit = validate_stored_automation_fixture(include_str!(
            "../../../../docs/topology-on-epic/topologies/readonly-fanout-audit.json"
        ));
        let audit_kinds = audit
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.as_str(),
                    node.step().expect("step").expect("typed step").kind_name(),
                )
            })
            .collect::<HashMap<_, _>>();
        for id in ["audit_1", "audit_2", "audit_3", "audit_4", "summary"] {
            assert_eq!(audit_kinds[id], "session");
        }
        let audit_sessions = audit
            .nodes
            .iter()
            .filter(|node| node.id.starts_with("audit_"))
            .count();
        assert_eq!(audit_sessions, 4);
        assert!(audit_sessions <= 6);
    }

    // ── Pure-fn validator tests (no daemon needed) ─────────────────────

    #[test]
    fn test_validate_acyclic_passes() {
        let def = TopologyDefinition {
            nodes: vec![
                node("a", SessionKind::Task),
                node("b", SessionKind::Task),
                node("c", SessionKind::Task),
            ],
            edges: vec![edge("a", "b", false), edge("b", "c", false)],
            until: None,
        };
        assert!(validate_topology_definition(&def).is_ok());
    }

    #[test]
    fn test_validate_cycle_without_loop_edge_rejected() {
        let def = TopologyDefinition {
            nodes: vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            edges: vec![edge("a", "b", false), edge("b", "a", false)],
            until: None,
        };
        assert_eq!(
            validate_topology_definition(&def).unwrap_err(),
            TopologyError::CycleDetected
        );
    }

    #[test]
    fn test_validate_cycle_with_loop_edge_passes() {
        // a→b acyclic; b→a is marked loop_edge so Kahn excludes it.
        // The full-edge SCC is still a loop region, which requires a
        // termination guard — we use until: Some(MaxIterations(5)) here.
        let def = TopologyDefinition {
            nodes: vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            edges: vec![edge("a", "b", false), edge("b", "a", true)],
            until: Some(UntilCondition::MaxIterations(5)),
        };
        assert!(validate_topology_definition(&def).is_ok());
    }

    #[test]
    fn test_validate_node_kind_epic_rejected() {
        let def = TopologyDefinition {
            nodes: vec![node("a", SessionKind::Epic)],
            edges: vec![],
            until: None,
        };
        assert_eq!(
            validate_topology_definition(&def).unwrap_err(),
            TopologyError::IllegalNodeKind(SessionKind::Epic)
        );
    }

    #[test]
    fn test_validate_edge_to_unknown_node_rejected() {
        let def = TopologyDefinition {
            nodes: vec![node("exists", SessionKind::Task)],
            edges: vec![edge("exists", "nope", false)],
            until: None,
        };
        assert_eq!(
            validate_topology_definition(&def).unwrap_err(),
            TopologyError::UnknownNode("nope".to_string())
        );
    }

    #[test]
    fn test_max_iterations_cap_rejected() {
        let mut n = node("a", SessionKind::Task);
        n.max_iterations = Some(33);
        let def = TopologyDefinition {
            nodes: vec![n],
            edges: vec![],
            until: None,
        };
        assert_eq!(
            validate_topology_definition(&def).unwrap_err(),
            TopologyError::IterationCapExceeded(33)
        );
    }

    #[test]
    fn test_loop_without_termination_rejected() {
        // SCC {a,b} via loop_edge: true, neither node has max_iterations,
        // def.until is None — unbounded loop.
        let def = TopologyDefinition {
            nodes: vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            edges: vec![edge("a", "b", false), edge("b", "a", true)],
            until: None,
        };
        assert_eq!(
            validate_topology_definition(&def).unwrap_err(),
            TopologyError::UnboundedLoop
        );
    }

    #[test]
    fn test_loop_with_max_iterations_passes() {
        let mut a = node("a", SessionKind::Task);
        a.max_iterations = Some(5);
        let def = TopologyDefinition {
            nodes: vec![a, node("b", SessionKind::Task)],
            edges: vec![edge("a", "b", false), edge("b", "a", true)],
            until: None,
        };
        assert!(validate_topology_definition(&def).is_ok());
    }

    #[test]
    fn test_loop_with_until_predicate_passes() {
        let def = TopologyDefinition {
            nodes: vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            edges: vec![edge("a", "b", false), edge("b", "a", true)],
            until: Some(UntilCondition::LeadHalt),
        };
        assert!(validate_topology_definition(&def).is_ok());
    }

    // ── SessionManager-level tests (TempDir + Store) ───────────────────

    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::Store;
    use tempfile::TempDir;

    fn manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        let config = Config::from_env();
        let runtime_config = RuntimeConfig::from_config(&config);
        let manager = SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("manager");
        (manager, dir)
    }

    fn sample_def() -> TopologyDefinition {
        TopologyDefinition {
            nodes: vec![node("only", SessionKind::Task)],
            edges: vec![],
            until: None,
        }
    }

    /// Poll get_topology until the persistence channel flushes the insert,
    /// or until 500ms elapses. Mirrors the labels-test wait pattern.
    async fn wait_for_topology(mgr: &SessionManager, id: Uuid) {
        for _ in 0..50 {
            if mgr.get_topology(id).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn test_create_topology_duplicate_name_rejected() {
        let (mgr, _dir) = manager();
        let first_id = mgr
            .create_topology("foo".to_string(), sample_def())
            .await
            .unwrap();
        wait_for_topology(&mgr, first_id).await;

        let err = mgr
            .create_topology("foo".to_string(), sample_def())
            .await
            .expect_err("second insert with same name must fail");
        match err {
            DaemonError::InvalidParam(msg) => assert!(msg.contains("already exists")),
            other => panic!("expected DuplicateName, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_create_topology_creates_bridged_workflow_row() {
        let (mgr, _dir) = manager();
        let topology_id = mgr
            .create_topology("picker-visible".to_string(), sample_def())
            .await
            .expect("create_topology");
        wait_for_topology(&mgr, topology_id).await;

        let workflow = mgr
            .get_workflow(crate::session::topology_bridge::derive_workflow_id(
                topology_id,
            ))
            .await
            .expect("get_workflow")
            .expect("bridged workflow row should exist");
        assert_eq!(workflow.title, "picker-visible");
        assert_eq!(workflow.project_id, None);
    }

    #[tokio::test]
    async fn test_update_topology_rename_to_existing_rejected() {
        let (mgr, _dir) = manager();
        let a_id = mgr
            .create_topology("a".to_string(), sample_def())
            .await
            .unwrap();
        let b_id = mgr
            .create_topology("b".to_string(), sample_def())
            .await
            .unwrap();
        wait_for_topology(&mgr, a_id).await;
        wait_for_topology(&mgr, b_id).await;

        let err = mgr
            .update_topology(a_id, Some("b".to_string()), None)
            .await
            .expect_err("rename to existing name must fail");
        match err {
            DaemonError::InvalidParam(msg) => assert!(msg.contains("already exists")),
            other => panic!("expected DuplicateName, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_delete_topology_unreferenced_succeeds() {
        let (mgr, _dir) = manager();
        let id = mgr
            .create_topology("disposable".to_string(), sample_def())
            .await
            .unwrap();
        wait_for_topology(&mgr, id).await;
        mgr.delete_topology(id).await.expect("delete must succeed");
        // Poll until the delete drains.
        for _ in 0..50 {
            if mgr.get_topology(id).await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let result = mgr.get_topology(id).await;
        assert!(matches!(result, Err(DaemonError::InvalidParam(_))));
    }

    #[tokio::test]
    async fn test_delete_topology_in_use_rejected() {
        let (mgr, _dir) = manager();
        let topology_id = mgr
            .create_topology("in-use".to_string(), sample_def())
            .await
            .unwrap();
        wait_for_topology(&mgr, topology_id).await;

        // Seed an Epic row referencing the topology via workflow_id. The
        // existing SessionManager API doesn't expose a direct path for this,
        // so we write the row via the raw store (mirrors the labels.rs unit
        // tests pattern, see hierarchy_ops.rs:788).
        let store = mgr.store.clone();
        let epic_id = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        let topology_id_str = topology_id.to_string();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .conn
                .execute(
                    "INSERT INTO sessions (
                        id, status, session_kind, provider, query, working_dir,
                        rotation_depth, created_at, updated_at, workflow_id
                    ) VALUES (?1, 'Completed', 'Epic', 'Claude', 'test-epic', '/',
                              0, ?2, ?2, ?3)",
                    rusqlite::params![epic_id.to_string(), now, topology_id_str],
                )
                .unwrap();
        })
        .await
        .unwrap();

        let err = mgr
            .delete_topology(topology_id)
            .await
            .expect_err("delete must be rejected");
        match err {
            DaemonError::InvalidParam(msg) => {
                assert!(
                    msg.contains("in use"),
                    "expected InUse message, got: {}",
                    msg
                )
            }
            other => panic!("expected InUse, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_list_topologies_filters_by_prefix() {
        let (mgr, _dir) = manager();
        let id1 = mgr
            .create_topology("alpha".to_string(), sample_def())
            .await
            .unwrap();
        let id2 = mgr
            .create_topology("alpine".to_string(), sample_def())
            .await
            .unwrap();
        let id3 = mgr
            .create_topology("beta".to_string(), sample_def())
            .await
            .unwrap();
        wait_for_topology(&mgr, id1).await;
        wait_for_topology(&mgr, id2).await;
        wait_for_topology(&mgr, id3).await;

        // Poll until the persistence channel drains.
        for _ in 0..50 {
            let all = mgr.list_topologies(None).await.unwrap();
            if all.len() == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let alp = mgr.list_topologies(Some("alp".to_string())).await.unwrap();
        assert_eq!(alp.len(), 2, "expected 2 alp* rows, got {}", alp.len());
        let names: Vec<&str> = alp.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"alpine"));
    }

    #[tokio::test]
    async fn test_topology_params_round_trip() {
        let (mgr, _dir) = manager();
        let mut n = node("work", SessionKind::Task);
        n.params.insert(
            "audience".to_string(),
            serde_json::Value::String("myself".to_string()),
        );
        n.params.insert(
            "model".to_string(),
            serde_json::Value::String("gpt-5".to_string()),
        );
        let def = TopologyDefinition {
            nodes: vec![n],
            edges: vec![],
            until: None,
        };
        let id = mgr
            .create_topology("with-params".into(), def.clone())
            .await
            .unwrap();
        wait_for_topology(&mgr, id).await;
        let loaded = mgr.get_topology(id).await.unwrap();
        assert_eq!(
            loaded.definition.nodes[0].params.get("audience"),
            Some(&serde_json::Value::String("myself".to_string())),
        );
        assert_eq!(
            loaded.definition.nodes[0].params.get("model"),
            Some(&serde_json::Value::String("gpt-5".to_string())),
        );
    }

    #[test]
    fn test_topology_node_missing_params_deserializes() {
        let json = r#"{
            "id": "legacy",
            "kind": "Task",
            "label": "Legacy node",
            "prereqs": []
        }"#;
        let node: TopologyNode = serde_json::from_str(json).unwrap();
        assert!(node.params.is_empty());
    }
}
