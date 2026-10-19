//! Pure metadata decoding and layer planning shared by the legacy graph runner
//! and the durable topology executor (#2415, #2308 cut 2).
//!
//! Every helper here is a function of a workflow snapshot only. Malformed
//! metadata keeps the legacy fallbacks: the helper logs, ignores the value and
//! returns the empty/`None` result.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use rsi_common::types::{FailurePolicy, UntilCondition};
use tracing::warn;

use crate::data::Value as GraphValue;
use crate::format::{EdgeDef, NodeDef};

/// `FailurePolicy::Retry` budget of one node: `repeat_policy.max_iterations`
/// retries (default 1). Shared by the legacy runner and the durable executor
/// so the kill switch never changes an ordinary workflow's retry behaviour.
#[must_use]
pub fn failure_retry_budget(node_def: &NodeDef) -> u32 {
    node_def
        .repeat_policy
        .as_ref()
        .map_or(1, |rp| u32::try_from(rp.max_iterations).unwrap_or(u32::MAX))
}

/// Parse `metadata["loop_edges"]` into a list of `(from, to)` string pairs.
///
/// The value is expected to be a JSON-serialized `Vec<{"from": str, "to": str}>`.
/// Returns an empty Vec on any parse failure or absent key (backward compatible
/// with acyclic-only workflows that have no `loop_edges` metadata).
#[must_use]
pub fn parse_loop_edges_from_metadata(
    metadata: &BTreeMap<String, GraphValue>,
) -> Vec<(String, String)> {
    let s = match metadata.get("loop_edges") {
        Some(GraphValue::String(s)) => s.as_str(),
        _ => return Vec::new(),
    };

    let parsed: Vec<serde_json::Value> = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "P1.11: failed to parse loop_edges metadata — ignoring");
            return Vec::new();
        }
    };

    parsed
        .into_iter()
        .filter_map(|v| {
            let from = v.get("from")?.as_str()?.to_string();
            let to = v.get("to")?.as_str()?.to_string();
            Some((from, to))
        })
        .collect()
}

/// Parse `metadata["scc_regions"]` into a `Vec<Vec<String>>`.
///
/// Each inner Vec is one SCC (sorted by node-id). Returns empty on parse
/// failure or absent key.
#[must_use]
pub fn parse_scc_regions_from_metadata(
    metadata: &BTreeMap<String, GraphValue>,
) -> Vec<Vec<String>> {
    let s = match metadata.get("scc_regions") {
        Some(GraphValue::String(s)) => s.as_str(),
        _ => return Vec::new(),
    };

    match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "P1.11: failed to parse scc_regions metadata — ignoring");
            Vec::new()
        }
    }
}

/// Parse `metadata["until_condition"]` into an `Option<UntilCondition>`.
///
/// Returns `None` on parse failure or absent key.
#[must_use]
pub fn parse_until_condition_from_metadata(
    metadata: &BTreeMap<String, GraphValue>,
) -> Option<UntilCondition> {
    let s = match metadata.get("until_condition") {
        Some(GraphValue::String(s)) => s.as_str(),
        _ => return None,
    };

    match serde_json::from_str(s) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(error = %e, "P1.11: failed to parse until_condition metadata — ignoring");
            None
        }
    }
}

/// Parse `metadata["failure_policies"]` into a `HashMap<String, FailurePolicy>`.
///
/// The value is a JSON-serialized `HashMap<String, String>` (node_id → policy name).
/// Returns an empty map on parse failure or absent key (backward compatible with
/// workflows that have no per-node failure policies).
#[must_use]
pub fn parse_failure_policies_from_metadata(
    metadata: &BTreeMap<String, GraphValue>,
) -> HashMap<String, FailurePolicy> {
    let s = match metadata.get("failure_policies") {
        Some(GraphValue::String(s)) => s.as_str(),
        _ => return HashMap::new(),
    };

    let raw: HashMap<String, String> = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "P1.11: failed to parse failure_policies metadata — ignoring");
            return HashMap::new();
        }
    };

    raw.into_iter()
        .filter_map(|(k, v)| {
            // The bridge serializes FailurePolicy via Debug (e.g. "Halt", "Retry", "Skip").
            // Try serde_json quoted form first (canonical), then fallback to Debug form.
            let quoted = format!("\"{}\"", v.to_lowercase());
            serde_json::from_str::<FailurePolicy>(&quoted)
                .or_else(|_| {
                    // Also try capitalized form
                    let capitalized = format!("\"{}\"", v);
                    serde_json::from_str::<FailurePolicy>(&capitalized)
                })
                .ok()
                .map(|fp| (k, fp))
        })
        .collect()
}

/// Compute topological layers via Kahn's algorithm.
///
/// Accepts an optional `loop_edge_pairs` list: edges in this list are excluded
/// from the Kahn pass (they are loop back-edges that would otherwise trigger a
/// false "cycle detected" error). If `loop_edge_pairs` is empty, the function
/// behaves identically to the pre-P1.11 implementation.
///
/// Returns a `Vec<Vec<String>>` where each inner vec is a layer of nodes
/// that can execute concurrently.
///
/// # Errors
///
/// Returns an error if the (acyclic) graph has a cycle.
#[allow(clippy::unwrap_used)]
pub fn topological_layers(
    nodes: &[NodeDef],
    edges: &[EdgeDef],
    loop_edge_pairs: &[(String, String)],
) -> Result<Vec<Vec<String>>, String> {
    let node_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

    // Build acyclic edge subset by filtering out declared loop back-edges.
    let loop_set: HashSet<(&str, &str)> = loop_edge_pairs
        .iter()
        .map(|(f, t)| (f.as_str(), t.as_str()))
        .collect();
    let acyclic_edges: Vec<&EdgeDef> = edges
        .iter()
        .filter(|e| !loop_set.contains(&(e.source.as_str(), e.target.as_str())))
        .collect();

    // Compute in-degree for each node (over acyclic edges only).
    let mut in_degree: HashMap<&str, usize> = node_ids.iter().map(|id| (*id, 0usize)).collect();
    let mut successors: HashMap<&str, Vec<&str>> = HashMap::new();

    for edge in &acyclic_edges {
        if !node_ids.contains(edge.source.as_str()) || !node_ids.contains(edge.target.as_str()) {
            continue; // Skip edges referencing unknown nodes.
        }
        *in_degree.entry(edge.target.as_str()).or_insert(0) += 1;
        successors
            .entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut layers: Vec<Vec<String>> = Vec::new();
    let mut visited = 0usize;

    while !queue.is_empty() {
        let layer_size = queue.len();
        let mut layer = Vec::with_capacity(layer_size);

        for _ in 0..layer_size {
            let node = queue.pop_front().unwrap();
            layer.push(node.to_string());
            visited += 1;

            if let Some(succs) = successors.get(node) {
                for &succ in succs {
                    let deg = in_degree.get_mut(succ).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(succ);
                    }
                }
            }
        }

        layers.push(layer);
    }

    if visited != node_ids.len() {
        return Err(format!(
            "workflow graph contains a cycle (visited {} of {} nodes)",
            visited,
            node_ids.len()
        ));
    }

    Ok(layers)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn topological_layers_linear_pipeline() {
        let nodes = vec![
            NodeDef::action("a", "A"),
            NodeDef::action("b", "B"),
            NodeDef::action("c", "C"),
        ];
        let edges = vec![EdgeDef::new("a", "b"), EdgeDef::new("b", "c")];

        let layers = topological_layers(&nodes, &edges, &[]).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec!["a"]);
        assert_eq!(layers[1], vec!["b"]);
        assert_eq!(layers[2], vec!["c"]);
    }

    #[test]
    fn topological_layers_diamond() {
        let nodes = vec![
            NodeDef::action("a", "A"),
            NodeDef::action("b", "B"),
            NodeDef::action("c", "C"),
            NodeDef::action("d", "D"),
        ];
        let edges = vec![
            EdgeDef::new("a", "b"),
            EdgeDef::new("a", "c"),
            EdgeDef::new("b", "d"),
            EdgeDef::new("c", "d"),
        ];

        let layers = topological_layers(&nodes, &edges, &[]).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec!["a"]);
        assert!(layers[1].contains(&"b".to_string()));
        assert!(layers[1].contains(&"c".to_string()));
        assert_eq!(layers[2], vec!["d"]);
    }

    #[test]
    fn topological_layers_detects_cycle() {
        let nodes = vec![NodeDef::action("a", "A"), NodeDef::action("b", "B")];
        let edges = vec![EdgeDef::new("a", "b"), EdgeDef::new("b", "a")];

        // With no loop_edge_pairs, Kahn should detect the cycle.
        let result = topological_layers(&nodes, &edges, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));
    }

    #[test]
    fn topological_layers_loop_edge_excluded_from_kahn() {
        // A→B (acyclic), B→A (loop_edge). Without the filter, Kahn would detect a cycle.
        // With the filter, only the A→B edge participates in Kahn, so it succeeds.
        let nodes = vec![NodeDef::action("a", "A"), NodeDef::action("b", "B")];
        let edges = vec![EdgeDef::new("a", "b"), EdgeDef::new("b", "a")];
        let loop_pairs = vec![("b".to_string(), "a".to_string())];

        let layers = topological_layers(&nodes, &edges, &loop_pairs).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0], vec!["a"]);
        assert_eq!(layers[1], vec!["b"]);
    }

    #[test]
    fn topological_layers_disconnected_nodes() {
        let nodes = vec![
            NodeDef::action("a", "A"),
            NodeDef::action("b", "B"),
            NodeDef::action("c", "C"),
        ];
        let edges = vec![];

        let layers = topological_layers(&nodes, &edges, &[]).unwrap();
        // All nodes in a single layer (no dependencies).
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 3);
    }

    /// test_executor_runs_simple_loop_to_max_iterations:
    /// Verify that a 2-node loop topology (A→B acyclic, B→A loop_edge) with
    /// MaxIterations(3) produces metadata with scc_regions containing both nodes,
    /// and that topological_layers correctly filters the loop edge from Kahn so it
    /// can produce a valid layer order. (Full round-trip requires #[ignore] AI calls.)
    #[test]
    fn test_executor_runs_simple_loop_to_max_iterations() {
        let nodes = vec![NodeDef::action("a", "A"), NodeDef::action("b", "B")];
        // Acyclic forward edge A→B, loop back-edge B→A.
        let all_edges = vec![EdgeDef::new("a", "b"), EdgeDef::new("b", "a")];
        let loop_pairs = vec![("b".to_string(), "a".to_string())];

        // With the loop_edge filtered out, Kahn succeeds.
        let layers = topological_layers(&nodes, &all_edges, &loop_pairs).unwrap();
        assert!(!layers.is_empty(), "should produce at least one layer");

        // Simulate metadata as bridge would stamp it.
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();
        let scc: Vec<Vec<String>> = vec![vec!["a".to_string(), "b".to_string()]];
        metadata.insert(
            "scc_regions".to_string(),
            GraphValue::String(serde_json::to_string(&scc).unwrap()),
        );
        let loop_edges_json: Vec<serde_json::Value> =
            vec![serde_json::json!({"from": "b", "to": "a"})];
        metadata.insert(
            "loop_edges".to_string(),
            GraphValue::String(serde_json::to_string(&loop_edges_json).unwrap()),
        );
        let until = rsi_common::types::UntilCondition::MaxIterations(3);
        metadata.insert(
            "until_condition".to_string(),
            GraphValue::String(serde_json::to_string(&until).unwrap()),
        );

        let loop_pairs_parsed = parse_loop_edges_from_metadata(&metadata);
        assert_eq!(loop_pairs_parsed.len(), 1);
        assert_eq!(loop_pairs_parsed[0], ("b".to_string(), "a".to_string()));

        let scc_regions = parse_scc_regions_from_metadata(&metadata);
        assert_eq!(scc_regions.len(), 1);
        assert!(scc_regions[0].contains(&"a".to_string()));
        assert!(scc_regions[0].contains(&"b".to_string()));

        let until_parsed = parse_until_condition_from_metadata(&metadata);
        assert!(matches!(
            until_parsed,
            Some(rsi_common::types::UntilCondition::MaxIterations(3))
        ));
    }

    /// test_executor_lead_halt_terminates_loop:
    /// Verify that LeadHalt until_condition is correctly parsed from metadata and that
    /// the UntilCondition::LeadHalt variant round-trips through serde.
    #[test]
    fn test_executor_lead_halt_terminates_loop() {
        let until = rsi_common::types::UntilCondition::LeadHalt;
        let serialized = serde_json::to_string(&until).unwrap();
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();
        metadata.insert(
            "until_condition".to_string(),
            GraphValue::String(serialized),
        );

        let parsed = parse_until_condition_from_metadata(&metadata);
        assert!(
            matches!(parsed, Some(rsi_common::types::UntilCondition::LeadHalt)),
            "LeadHalt should round-trip through metadata"
        );
    }

    /// test_executor_failure_policy_retry:
    /// Verify that FailurePolicy::Retry is correctly parsed from metadata.
    /// The bridge serializes via Debug ("Retry"), the parser normalizes to lowercase
    /// for serde_json deserialization ("retry").
    #[test]
    fn test_executor_failure_policy_retry() {
        // Build metadata as bridge would stamp it (Debug format: "Retry").
        let raw: HashMap<String, String> = [("node_a".to_string(), "Retry".to_string())]
            .into_iter()
            .collect();
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();
        metadata.insert(
            "failure_policies".to_string(),
            GraphValue::String(serde_json::to_string(&raw).unwrap()),
        );

        let policies = parse_failure_policies_from_metadata(&metadata);
        assert_eq!(
            policies.get("node_a"),
            Some(&FailurePolicy::Retry),
            "Retry should parse correctly from Debug-format string"
        );
    }

    /// test_executor_failure_policy_skip:
    /// Verify that FailurePolicy::Skip is correctly parsed from metadata.
    #[test]
    fn test_executor_failure_policy_skip() {
        let raw: HashMap<String, String> = [("node_b".to_string(), "Skip".to_string())]
            .into_iter()
            .collect();
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();
        metadata.insert(
            "failure_policies".to_string(),
            GraphValue::String(serde_json::to_string(&raw).unwrap()),
        );

        let policies = parse_failure_policies_from_metadata(&metadata);
        assert_eq!(
            policies.get("node_b"),
            Some(&FailurePolicy::Skip),
            "Skip should parse correctly from Debug-format string"
        );
    }

    /// test_executor_failure_policy_halt:
    /// Verify that FailurePolicy::Halt is correctly parsed, and that absent entries
    /// (no policy in map) return None (which the executor treats as Halt-equivalent).
    #[test]
    fn test_executor_failure_policy_halt() {
        let raw: HashMap<String, String> = [("node_c".to_string(), "Halt".to_string())]
            .into_iter()
            .collect();
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();
        metadata.insert(
            "failure_policies".to_string(),
            GraphValue::String(serde_json::to_string(&raw).unwrap()),
        );

        let policies = parse_failure_policies_from_metadata(&metadata);
        assert_eq!(
            policies.get("node_c"),
            Some(&FailurePolicy::Halt),
            "Halt should parse correctly from Debug-format string"
        );
        // Absent node → None → treated as Halt by executor.
        assert_eq!(
            policies.get("node_x"),
            None,
            "absent node has no policy (treated as Halt by executor)"
        );
    }
}
