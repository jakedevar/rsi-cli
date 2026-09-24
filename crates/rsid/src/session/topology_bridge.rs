//! Topology → WorkflowDefinition bridge (P1.10/P1.11).
//!
//! Pure translation layer: converts a `Topology` (stored in the `topologies`
//! SQLite table) into a `WorkflowDefinition` consumable by `graph_runner.rs`.
//!
//! # Invariants
//!
//! - `bridge_topology_to_workflow` is **pure** (no async, no DB). All I/O is
//!   owned by the RPC handler (`handle_execute_topology` in `rpc.rs`).
//! - Loop edges supported (P1.11): `loop_edge: true` edges are collected and
//!   stamped into metadata as `loop_edges` + `scc_regions` + `until_condition`.
//!   The executor reads these keys to run the loop-aware Kahn pass.
//! - `until` conditions are stamped as structured `until_condition` JSON (P1.11)
//!   AND as a legacy `until_predicate` warning string for backward compat.
//! - `prereqs` that have no corresponding edge cause a hard fail via
//!   `BridgeError::PrereqsWithoutEdges`. Silent wrong order is worse than
//!   a clear error.

use crate::error::DaemonError;
use crate::session::topology_ops::compute_scc_regions;
use rsi_common::types::{Topology, Workflow, WorkflowDocument, WorkflowStage};
use rsi_graph::data::Value as GraphValue;
use rsi_graph::format::{EdgeDef, ModelSettings, NodeDef, RepeatPolicy, WorkflowDefinition};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

// ─── P1.12 §9: bridge-side workflows-table upsert ────────────────────────────

/// Stable namespace for v5-derived workflow ids that mirror topologies.
///
/// **MUST NEVER CHANGE across releases** — the v5 algebra depends on a fixed
/// namespace UUID. Bumping it would break idempotency for every already-
/// mirrored topology (new id ≠ old id), creating orphan duplicates.
const BRIDGE_NAMESPACE_UUID: Uuid = uuid::uuid!("c5c5f8d6-3d6a-4f5e-9b4e-1f8c3a7d6b9e");

/// Derive the deterministic workflow row id for a given topology.
///
/// The bridge mirrors each topology into the `workflows` table for `gv`-picker
/// visibility. The mirrored row's primary key is `Uuid::new_v5(NS, topology_id)`
/// so the upsert path is idempotent via `ON CONFLICT(id) DO UPDATE`.
pub(crate) fn derive_workflow_id(topology_id: Uuid) -> Uuid {
    Uuid::new_v5(&BRIDGE_NAMESPACE_UUID, topology_id.as_bytes())
}

// ─── BridgeError ─────────────────────────────────────────────────────────────

/// Errors that may occur when translating a `Topology` to a `WorkflowDefinition`.
///
/// All variants map to `DaemonError::InvalidParam` via `From`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeError {
    InvalidNodeParam {
        node_id: String,
        param: String,
    },
    /// A node declares prereqs that have no corresponding incoming edge.
    ///
    /// This is a hard-fail: silently producing wrong execution order (the
    /// executor would skip the missing prerequisite entirely) is a correctness
    /// failure.
    PrereqsWithoutEdges {
        node_id: String,
        missing_prereqs: Vec<String>,
    },
    /// A typed step, edge routing, or refused author parameter (#635).
    InvalidStep(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::InvalidNodeParam { node_id, param } => {
                write!(f, "node {node_id} has invalid {param}")
            }
            BridgeError::PrereqsWithoutEdges {
                node_id,
                missing_prereqs,
            } => {
                write!(
                    f,
                    "node {node_id} declares prereqs with no corresponding incoming edge: {}",
                    missing_prereqs.join(", ")
                )
            }
            Self::InvalidStep(message) => f.write_str(message),
        }
    }
}

impl From<BridgeError> for DaemonError {
    fn from(err: BridgeError) -> Self {
        DaemonError::InvalidParam(err.to_string())
    }
}

// ─── Translation ─────────────────────────────────────────────────────────────

use super::SessionManager;

impl SessionManager {
    /// Translate a stored `Topology` into a `WorkflowDefinition` for the
    /// `graph_runner` async executor.
    ///
    /// This function is **pure** — it does not touch the database or any async
    /// runtime. All I/O (topology load, executor call) lives in the caller.
    ///
    /// # Translation algorithm
    ///
    /// 1. **Pass 1 — loop-edge collection**: collect `loop_edge: true` edges for
    ///    metadata stamping. These no longer cause a rejection (P1.11).
    /// 2. **Pass 2 — prereq validation**: for each node with non-empty
    ///    `prereqs`, verify each prereq appears as a source for an edge leading
    ///    into this node. Missing edge → `Err(BridgeError::PrereqsWithoutEdges)`.
    /// 3. **Pass 3 — node translation**: map per-node fields, params, tags.
    /// 4. **Pass 4 — edge translation**: `TopologyEdge { from, to }` →
    ///    `EdgeDef { source, target }` (all edges, including loop_edge: true).
    /// 5. **Pass 5 — `until` handling**: stamp structured `until_condition` JSON
    ///    AND legacy `until_predicate` warning string for backward compat.
    /// 6. **Pass 6 — WorkflowDefinition assembly**: build the final struct with
    ///    provenance metadata stamps (including `loop_edges`, `scc_regions`).
    pub(crate) fn bridge_topology_to_workflow(
        &self,
        topology: &Topology,
    ) -> Result<WorkflowDefinition, BridgeError> {
        let def = &topology.definition;
        // #635: typed steps are re-validated on every bridge (upsert and
        // execute) and travel in metadata, never as tags.
        crate::topology::steps::validate_topology(def).map_err(BridgeError::InvalidStep)?;
        let step_metadata =
            crate::topology::steps::bridge_metadata(def).map_err(BridgeError::InvalidStep)?;

        // ── Pass 1: loop-edge collection (P1.11: no longer rejected) ─────
        let loop_edges: Vec<(String, String)> = def
            .edges
            .iter()
            .filter(|e| e.loop_edge)
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();

        // Compute SCC regions for loop-bearing topologies.
        let scc_regions = if !loop_edges.is_empty() {
            compute_scc_regions(def)
        } else {
            Vec::new()
        };

        // ── Pass 2: prereq validation ─────────────────────────────────────
        // Build a set of (from, to) pairs from all edges (including loop edges)
        // so we can check that every prereq declared on a node has a real
        // incoming edge.
        let edge_pairs: HashSet<(&str, &str)> = def
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();

        for node in &def.nodes {
            if node.prereqs.is_empty() {
                continue;
            }
            let missing: Vec<String> = node
                .prereqs
                .iter()
                .filter(|prereq| !edge_pairs.contains(&(prereq.as_str(), node.id.as_str())))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Err(BridgeError::PrereqsWithoutEdges {
                    node_id: node.id.clone(),
                    missing_prereqs: missing,
                });
            }
        }

        // ── Collect metadata sidecar values before building nodes ─────────
        // failure_policy_map: node_id → policy string (for metadata stamp)
        let mut failure_policy_map: HashMap<String, String> = HashMap::new();
        // prereqs_map: node_id → prereq list (for metadata stamp)
        let mut prereqs_map: HashMap<String, Vec<String>> = HashMap::new();

        // ── Pass 3: node translation ──────────────────────────────────────
        let mut translated_nodes: Vec<NodeDef> = Vec::with_capacity(def.nodes.len());

        for node in &def.nodes {
            let mut node_def = NodeDef::action(node.id.clone(), node.label.clone());

            // kind → tag (always)
            node_def.tags.push(format!("kind:{:?}", node.kind));

            // params extraction
            let mut unknown_params: Vec<(String, &serde_json::Value)> = Vec::new();
            for (key, val) in &node.params {
                match key.as_str() {
                    "instructions" => {
                        if let Some(s) = val.as_str() {
                            node_def.instructions = s.to_string();
                        }
                    }
                    "provider" => {
                        if let Some(s) = val.as_str() {
                            node_def.provider = Some(s.to_string());
                        }
                    }
                    "model" => {
                        if let Some(s) = val.as_str() {
                            let settings =
                                node_def
                                    .model_settings
                                    .get_or_insert_with(|| ModelSettings {
                                        model: None,
                                        max_tokens: None,
                                        temperature: None,
                                        top_p: None,
                                        provider: None,
                                    });
                            settings.model = Some(s.to_string());
                        }
                    }
                    "temperature" => {
                        if let Some(t) = val.as_f64() {
                            let settings =
                                node_def
                                    .model_settings
                                    .get_or_insert_with(|| ModelSettings {
                                        model: None,
                                        max_tokens: None,
                                        temperature: None,
                                        top_p: None,
                                        provider: None,
                                    });
                            settings.temperature = Some(t);
                        }
                    }
                    "working_dir" => {
                        if let Some(s) = val.as_str() {
                            node_def.working_dir = Some(std::path::PathBuf::from(s));
                        }
                    }
                    "sandbox" => {
                        if val.as_bool() != Some(true) {
                            return Err(BridgeError::InvalidNodeParam {
                                node_id: node.id.clone(),
                                param: "sandbox (must be true)".into(),
                            });
                        }
                        node_def.sandbox = true;
                    }
                    "effort" => {
                        let Some(effort) = val.as_str() else {
                            return Err(BridgeError::InvalidNodeParam {
                                node_id: node.id.clone(),
                                param: "effort".into(),
                            });
                        };
                        node_def.tags.push(format!("effort={effort}"));
                    }
                    "custody" => {
                        let Some(from) = val.get("from").and_then(serde_json::Value::as_str) else {
                            return Err(BridgeError::InvalidNodeParam {
                                node_id: node.id.clone(),
                                param: "custody.from".into(),
                            });
                        };
                        node_def.tags.push(format!("custody.from={from}"));
                    }
                    "tags" => {
                        if let Some(arr) = val.as_array() {
                            for tag_val in arr {
                                if let Some(t) = tag_val.as_str() {
                                    node_def.tags.push(t.to_string());
                                }
                            }
                        }
                    }
                    key if crate::topology::steps::is_step_param(key) => {}
                    _ => {
                        unknown_params.push((key.clone(), val));
                    }
                }
            }

            // Remaining unknown params → "key=value" tags
            for (k, v) in unknown_params {
                let v_str = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                node_def.tags.push(format!("{}={}", k, v_str));
            }

            // max_iterations → repeat_policy
            if let Some(max_iter) = node.max_iterations {
                node_def.repeat_policy = Some(RepeatPolicy {
                    max_iterations: max_iter as usize,
                    termination: None,
                });
            }

            // Collect sidecar metadata
            if let Some(policy) = node.on_failure {
                failure_policy_map.insert(node.id.clone(), format!("{:?}", policy));
            }
            if !node.prereqs.is_empty() {
                prereqs_map.insert(node.id.clone(), node.prereqs.clone());
            }

            translated_nodes.push(node_def);
        }

        // ── Pass 4: edge translation ──────────────────────────────────────
        let translated_edges: Vec<EdgeDef> = def
            .edges
            .iter()
            .map(|e| EdgeDef::new(e.from.clone(), e.to.clone()))
            .collect();

        // ── Pass 5: until handling (P1.11: structured + legacy warning) ──
        // Stamp both structured until_condition JSON (for executor) and
        // legacy until_predicate warning string (for backward compat with
        // existing tests that check for until_predicate key).
        let until_structured: Option<String> = def
            .until
            .as_ref()
            .and_then(|until_cond| serde_json::to_string(until_cond).ok());
        let until_warning: Option<(String, String)> = def.until.as_ref().map(|until_cond| {
            let predicate_str = format!("{:?}", until_cond);
            let warning_str =
                "until_predicate legacy compat — see until_condition for structured value"
                    .to_string();
            (predicate_str, warning_str)
        });

        // ── Pass 6: WorkflowDefinition assembly ───────────────────────────
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();

        metadata.insert(
            "source_topology_id".to_string(),
            GraphValue::String(topology.id.to_string()),
        );
        metadata.insert(
            "source_topology_name".to_string(),
            GraphValue::String(topology.name.clone()),
        );
        metadata.insert(
            "bridged_at".to_string(),
            GraphValue::String(chrono::Utc::now().to_rfc3339()),
        );

        if !failure_policy_map.is_empty() {
            let fp_json = serde_json::to_string(&failure_policy_map).unwrap_or_default();
            metadata.insert("failure_policies".to_string(), GraphValue::String(fp_json));
        }

        if !prereqs_map.is_empty() {
            let prereqs_json = serde_json::to_string(&prereqs_map).unwrap_or_default();
            metadata.insert("prereqs".to_string(), GraphValue::String(prereqs_json));
        }

        if let Some((predicate_str, warning_str)) = until_warning {
            metadata.insert(
                "until_predicate".to_string(),
                GraphValue::String(predicate_str),
            );
            metadata.insert(
                "until_predicate_warning".to_string(),
                GraphValue::String(warning_str),
            );
        }

        // ── P1.11: loop-edge and SCC metadata stamps ──────────────────────
        if !loop_edges.is_empty() {
            // Stamp loop_edges as JSON: [{from: "a", to: "b"}, ...]
            let loop_edge_json_vals: Vec<serde_json::Value> = loop_edges
                .iter()
                .map(|(from, to)| serde_json::json!({"from": from, "to": to}))
                .collect();
            metadata.insert(
                "loop_edges".to_string(),
                GraphValue::String(serde_json::to_string(&loop_edge_json_vals).unwrap_or_default()),
            );
            // Stamp scc_regions as JSON: [["a", "b"], ["c"]]
            metadata.insert(
                "scc_regions".to_string(),
                GraphValue::String(serde_json::to_string(&scc_regions).unwrap_or_default()),
            );
        }

        // Stamp structured until_condition (P1.11) if present.
        if let Some(until_json) = until_structured {
            metadata.insert(
                "until_condition".to_string(),
                GraphValue::String(until_json),
            );
        }

        metadata.extend(step_metadata);

        Ok(WorkflowDefinition {
            version: "1.0".to_string(),
            name: topology.name.clone(),
            description: String::new(),
            nodes: translated_nodes,
            edges: translated_edges,
            metadata,
        })
    }

    /// Mirror a topology into the `workflows` table via deterministic-id upsert
    /// (P1.12 §9). Returns the resolved workflow row id.
    ///
    /// Called on three trigger points by the RPC layer:
    ///   - `ExecuteTopology` — guarantees gv-picker visibility before launch
    ///   - `UpdateTopology`  — re-bridges so picker entry stays fresh
    ///   - `DeleteTopology`  — paired with `delete_workflow_by_source_topology`
    ///
    /// Idempotency: the workflow row id is `derive_workflow_id(topology.id)`
    /// (v5-derived). `ON CONFLICT(id) DO UPDATE` discards stale `created_at`
    /// on collision; `title`, `stage`, `project_id`, `definition_json`,
    /// `updated_at` are refreshed.
    pub(crate) async fn upsert_bridged_workflow(
        &self,
        topology: &Topology,
        project_id: Option<Uuid>,
    ) -> std::result::Result<Uuid, DaemonError> {
        let workflow_def = self
            .bridge_topology_to_workflow(topology)
            .map_err(DaemonError::from)?;
        let workflow_id = derive_workflow_id(topology.id);
        let resolved_project_id = match project_id {
            Some(project_id) => Some(project_id),
            None => self
                .get_workflow(workflow_id)
                .await?
                .and_then(|workflow| workflow.project_id),
        };
        let now = chrono::Utc::now();
        let workflow = Workflow {
            id: workflow_id,
            title: topology.name.clone(),
            stage: WorkflowStage::PlanComplete,
            artifact_path: None,
            project_id: resolved_project_id,
            created_at: now, // discarded on INSERT…ON CONFLICT(id) DO UPDATE
            updated_at: now,
        };
        let definition_value = serde_json::to_value(&workflow_def)
            .map_err(|e| DaemonError::Rpc(format!("bridge serialize: {e}")))?;
        let document = WorkflowDocument {
            workflow,
            definition: definition_value,
        };
        self.upsert_workflow_definition(document).await?;
        Ok(workflow_id)
    }

    /// Re-bridge every stored topology into the `workflows` table.
    ///
    /// Used at daemon startup to repair pre-P1.13 databases where the source
    /// topology exists but the mirrored workflow row was never created. When a
    /// bridged row already exists, any stored project binding is preserved.
    pub async fn reconcile_topology_workflows(&self) -> std::result::Result<usize, DaemonError> {
        let topologies = self.list_topologies(None).await?;
        let total = topologies.len();
        for topology in topologies {
            self.upsert_bridged_workflow(&topology, None).await?;
        }
        Ok(total)
    }

    /// Cascade-delete the workflow row mirrored from `topology_id` (P1.12 §9).
    ///
    /// Idempotent: returns `Ok(())` whether or not the row existed. Called by
    /// `handle_delete_topology` BEFORE `delete_topology` (OQ2 ordering: workflow
    /// row first, then topology row, so a mid-operation failure leaves the
    /// workflow absent — recoverable via next `ExecuteTopology` — and the
    /// topology intact (source of truth survives)).
    pub(crate) async fn delete_workflow_by_source_topology(
        &self,
        topology_id: Uuid,
    ) -> std::result::Result<(), DaemonError> {
        let workflow_id = derive_workflow_id(topology_id);
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.delete_workflow(workflow_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::Store;
    use chrono::Utc;
    use rsi_common::types::{
        FailurePolicy, SessionKind, Topology, TopologyDefinition, TopologyEdge, TopologyNode,
        UntilCondition,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;
    use uuid::Uuid;

    // ── Local test helpers ────────────────────────────────────────────────

    /// Build a minimal SessionManager backed by a temp SQLite store.
    /// The returned TempDir MUST be bound in the caller to avoid early drop.
    fn manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        let config = Config::from_env();
        let runtime_config = RuntimeConfig::from_config(&config);
        let mgr = SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("manager");
        (mgr, dir)
    }

    /// Minimal node with no params, no prereqs, no max_iterations.
    fn node(id: &str, kind: SessionKind) -> TopologyNode {
        TopologyNode {
            id: id.to_string(),
            kind,
            label: id.to_string(),
            prereqs: vec![],
            max_iterations: None,
            on_failure: None,
            params: HashMap::new(),
        }
    }

    /// Node with custom params.
    fn node_with_params(
        id: &str,
        kind: SessionKind,
        params: HashMap<String, serde_json::Value>,
    ) -> TopologyNode {
        TopologyNode {
            id: id.to_string(),
            kind,
            label: id.to_string(),
            prereqs: vec![],
            max_iterations: None,
            on_failure: None,
            params,
        }
    }

    fn edge(from: &str, to: &str, loop_edge: bool) -> TopologyEdge {
        TopologyEdge {
            from: from.to_string(),
            to: to.to_string(),
            loop_edge,
        }
    }

    fn topology_with(nodes: Vec<TopologyNode>, edges: Vec<TopologyEdge>) -> Topology {
        let now = Utc::now();
        Topology {
            id: Uuid::new_v4(),
            name: "test".to_string(),
            definition: TopologyDefinition {
                nodes,
                edges,
                until: None,
            },
            created_at: now,
            updated_at: now,
        }
    }

    fn topology_with_until(
        nodes: Vec<TopologyNode>,
        edges: Vec<TopologyEdge>,
        until: Option<UntilCondition>,
    ) -> Topology {
        let now = Utc::now();
        Topology {
            id: Uuid::new_v4(),
            name: "test".to_string(),
            definition: TopologyDefinition {
                nodes,
                edges,
                until,
            },
            created_at: now,
            updated_at: now,
        }
    }

    // ── Unit tests — need tokio because SessionManager::new spawns background tasks ──

    #[tokio::test]
    async fn t1_a3_sandbox_false_is_invalid_before_launch() {
        let repo = TempDir::new().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let (mgr, _dir) = manager();
        let topo = topology_with(
            vec![node_with_params(
                "A",
                SessionKind::Task,
                HashMap::from([("sandbox".into(), serde_json::Value::Bool(false))]),
            )],
            vec![],
        );
        let error: DaemonError = mgr.bridge_topology_to_workflow(&topo).unwrap_err().into();
        assert!(matches!(error, DaemonError::InvalidParam(_)));
    }

    #[tokio::test]
    async fn test_bridge_acyclic_succeeds() {
        let (mgr, _dir) = manager();
        let topo = topology_with(
            vec![
                node("a", SessionKind::Task),
                node("b", SessionKind::Task),
                node("c", SessionKind::Task),
            ],
            vec![edge("a", "b", false), edge("b", "c", false)],
        );
        let wf = mgr
            .bridge_topology_to_workflow(&topo)
            .expect("should succeed");
        assert_eq!(wf.nodes.len(), 3);
        assert_eq!(wf.edges.len(), 2);
    }

    /// P1.11: inverted from P1.10 "loop edge must fail" — now loop edges produce
    /// metadata-stamped Ok(workflow) instead of Err(LoopEdgeUnsupported).
    #[tokio::test]
    async fn test_bridge_loop_edge_rejected() {
        let (mgr, _dir) = manager();
        let topo = topology_with(
            vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            vec![edge("a", "b", false), edge("b", "a", true)],
        );
        let wf = mgr
            .bridge_topology_to_workflow(&topo)
            .expect("P1.11: loop edges must produce Ok(workflow) with metadata stamps");
        assert!(
            wf.metadata.contains_key("loop_edges"),
            "metadata must contain loop_edges key"
        );
        assert!(
            wf.metadata.contains_key("scc_regions"),
            "metadata must contain scc_regions key"
        );
        // Both edges must be in the workflow (loop edges pass through).
        assert_eq!(wf.edges.len(), 2);
    }

    #[tokio::test]
    async fn test_bridge_until_predicate_warns_not_errors() {
        let (mgr, _dir) = manager();
        let topo = topology_with_until(
            vec![node("a", SessionKind::Task)],
            vec![],
            Some(UntilCondition::LeadHalt),
        );
        let wf = mgr
            .bridge_topology_to_workflow(&topo)
            .expect("until predicate must not error");
        assert!(
            wf.metadata.contains_key("until_predicate"),
            "metadata must contain until_predicate"
        );
        assert!(
            wf.metadata.contains_key("until_predicate_warning"),
            "metadata must contain until_predicate_warning"
        );
    }

    #[tokio::test]
    async fn test_bridge_prereqs_without_edges_errors() {
        let (mgr, _dir) = manager();
        // Node "b" declares prereq "a" but there is no edge a→b.
        let mut b = node("b", SessionKind::Task);
        b.prereqs = vec!["a".to_string()];
        let topo = topology_with(
            vec![node("a", SessionKind::Task), b],
            vec![], // no edge a→b
        );
        let err = mgr
            .bridge_topology_to_workflow(&topo)
            .expect_err("prereqs without edges must fail");
        match err {
            BridgeError::PrereqsWithoutEdges {
                node_id,
                missing_prereqs,
            } => {
                assert_eq!(node_id, "b");
                assert_eq!(missing_prereqs, vec!["a".to_string()]);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_bridge_params_provider_to_node_def() {
        let (mgr, _dir) = manager();
        let mut params = HashMap::new();
        params.insert(
            "provider".to_string(),
            serde_json::Value::String("codex".to_string()),
        );
        let topo = topology_with(
            vec![node_with_params("a", SessionKind::Task, params)],
            vec![],
        );
        let wf = mgr.bridge_topology_to_workflow(&topo).expect("ok");
        assert_eq!(wf.nodes[0].provider, Some("codex".to_string()));
    }

    #[tokio::test]
    async fn test_bridge_params_unknown_keys_to_tags() {
        let (mgr, _dir) = manager();
        let mut params = HashMap::new();
        params.insert(
            "custom_key".to_string(),
            serde_json::Value::String("value".to_string()),
        );
        let topo = topology_with(
            vec![node_with_params("a", SessionKind::Task, params)],
            vec![],
        );
        let wf = mgr.bridge_topology_to_workflow(&topo).expect("ok");
        assert!(
            wf.nodes[0].tags.contains(&"custom_key=value".to_string()),
            "unknown param must appear as tag: got {:?}",
            wf.nodes[0].tags
        );
    }

    #[tokio::test]
    async fn test_bridge_kind_encoded_as_tag() {
        let (mgr, _dir) = manager();
        let topo = topology_with(vec![node("a", SessionKind::Story)], vec![]);
        let wf = mgr.bridge_topology_to_workflow(&topo).expect("ok");
        assert!(
            wf.nodes[0].tags.contains(&"kind:Story".to_string()),
            "kind tag missing: {:?}",
            wf.nodes[0].tags
        );
    }

    #[tokio::test]
    async fn test_bridge_max_iterations_to_repeat_policy() {
        let (mgr, _dir) = manager();
        let mut n = node("a", SessionKind::Task);
        n.max_iterations = Some(3);
        let topo = topology_with(vec![n], vec![]);
        let wf = mgr.bridge_topology_to_workflow(&topo).expect("ok");
        let policy = wf.nodes[0]
            .repeat_policy
            .as_ref()
            .expect("repeat_policy must be set");
        assert_eq!(policy.max_iterations, 3);
    }

    #[tokio::test]
    async fn test_bridge_metadata_provenance_stamped() {
        let (mgr, _dir) = manager();
        let topo = topology_with(vec![node("a", SessionKind::Task)], vec![]);
        let wf = mgr.bridge_topology_to_workflow(&topo).expect("ok");
        assert!(wf.metadata.contains_key("source_topology_id"));
        assert!(wf.metadata.contains_key("source_topology_name"));
        assert!(wf.metadata.contains_key("bridged_at"));
    }

    #[tokio::test]
    async fn test_bridge_failure_policy_in_metadata() {
        let (mgr, _dir) = manager();
        let mut n = node("a", SessionKind::Task);
        n.on_failure = Some(FailurePolicy::Retry);
        let topo = topology_with(vec![n], vec![]);
        let wf = mgr.bridge_topology_to_workflow(&topo).expect("ok");
        let fp_val = wf
            .metadata
            .get("failure_policies")
            .expect("failure_policies in metadata");
        if let GraphValue::String(s) = fp_val {
            assert!(s.contains("Retry"), "policy string must contain Retry: {s}");
        } else {
            panic!("failure_policies must be a String value");
        }
    }

    // ── Integration tests (async, use manager()) ──────────────────────────

    #[tokio::test]
    async fn test_execute_topology_round_trip() {
        let (mgr, _dir) = manager();

        // Build a 3-node acyclic topology.
        let def = TopologyDefinition {
            nodes: vec![
                node("research", SessionKind::Task),
                node("plan", SessionKind::Task),
                node("implement", SessionKind::Task),
            ],
            edges: vec![
                edge("research", "plan", false),
                edge("plan", "implement", false),
            ],
            until: None,
        };

        let topology_id = mgr
            .create_topology("round-trip-test".to_string(), def)
            .await
            .expect("create_topology");

        // Wait for persistence to flush.
        for _ in 0..50 {
            if mgr.get_topology(topology_id).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let topo = mgr
            .get_topology(topology_id)
            .await
            .expect("get_topology after wait");

        // Bridge must succeed and produce correct shape.
        let wf = mgr
            .bridge_topology_to_workflow(&topo)
            .expect("bridge_topology_to_workflow");

        assert_eq!(wf.nodes.len(), 3);
        assert_eq!(wf.edges.len(), 2);
        assert_eq!(wf.name, "round-trip-test");

        // Provenance stamps present.
        assert!(wf.metadata.contains_key("source_topology_id"));
        assert!(wf.metadata.contains_key("source_topology_name"));
        assert!(wf.metadata.contains_key("bridged_at"));
    }

    /// P1.11: inverted from P1.10 "loop edge RPC returns InvalidParam" — now the
    /// bridge succeeds with metadata stamps, so the RPC path returns Ok with
    /// loop_edges and scc_regions populated in the workflow metadata.
    #[tokio::test]
    async fn test_bridge_loop_edge_rpc_returns_invalid_param() {
        let (mgr, _dir) = manager();

        // Create a topology with a loop edge — P1.11 must bridge successfully.
        let topo = topology_with(
            vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            vec![edge("a", "b", false), edge("b", "a", true)],
        );

        let wf = mgr
            .bridge_topology_to_workflow(&topo)
            .expect("P1.11: loop edges must succeed — bridge stamps metadata instead of rejecting");

        // Loop metadata must be present.
        assert!(
            wf.metadata.contains_key("loop_edges"),
            "metadata must contain loop_edges key"
        );
        assert!(
            wf.metadata.contains_key("scc_regions"),
            "metadata must contain scc_regions key"
        );

        // Verify the loop_edges value is parseable JSON with the right edge.
        if let Some(rsi_graph::data::Value::String(s)) = wf.metadata.get("loop_edges") {
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(s).expect("loop_edges must be valid JSON");
            assert_eq!(parsed.len(), 1, "one loop edge expected");
            assert_eq!(parsed[0]["from"], "b");
            assert_eq!(parsed[0]["to"], "a");
        } else {
            panic!("loop_edges metadata must be a String value");
        }
    }

    // ── P1.12 §9: bridge-side workflows-table upsert tests ────────────────

    /// `upsert_bridged_workflow` creates a workflow row on first call against
    /// a fresh topology. Row id is `derive_workflow_id(topology.id)` so the
    /// upsert is idempotent (verified separately in the next test).
    #[tokio::test]
    async fn test_execute_topology_upserts_workflow_row() {
        let (mgr, _dir) = manager();
        let topology = topology_with(
            vec![node("a", SessionKind::Task), node("b", SessionKind::Task)],
            vec![edge("a", "b", false)],
        );

        // Baseline: no workflows in DB.
        let pre = mgr.list_workflows(None).await.expect("list_workflows pre");
        assert_eq!(pre.len(), 0);

        let returned_id = mgr
            .upsert_bridged_workflow(&topology, None)
            .await
            .expect("upsert ok");
        assert_eq!(returned_id, derive_workflow_id(topology.id));

        let post = mgr.list_workflows(None).await.expect("list_workflows post");
        assert_eq!(post.len(), 1, "exactly one workflow row after upsert");
        assert_eq!(post[0].id, derive_workflow_id(topology.id));
        assert_eq!(post[0].title, topology.name);
    }

    /// Calling `upsert_bridged_workflow` twice against the same topology
    /// results in exactly one row in the `workflows` table — confirms the
    /// `ON CONFLICT(id) DO UPDATE` upsert path.
    #[tokio::test]
    async fn test_execute_topology_upsert_is_idempotent() {
        let (mgr, _dir) = manager();
        let topology = topology_with(vec![node("a", SessionKind::Task)], vec![]);

        mgr.upsert_bridged_workflow(&topology, None)
            .await
            .expect("first upsert");
        mgr.upsert_bridged_workflow(&topology, None)
            .await
            .expect("second upsert");

        let rows = mgr.list_workflows(None).await.expect("list_workflows");
        assert_eq!(rows.len(), 1, "idempotent upsert must keep exactly one row");
        assert_eq!(rows[0].id, derive_workflow_id(topology.id));
    }

    /// `handle_update_topology`'s re-bridge step refreshes the workflow row's
    /// title when the topology is renamed. We simulate the handler by calling
    /// `upsert_bridged_workflow` again with a renamed topology that shares
    /// the same id.
    #[tokio::test]
    async fn test_update_topology_cascades_to_workflow_row() {
        let (mgr, _dir) = manager();
        let mut topology = topology_with(vec![node("a", SessionKind::Task)], vec![]);
        topology.name = "old-name".to_string();

        mgr.upsert_bridged_workflow(&topology, None)
            .await
            .expect("first upsert");

        // Simulate handle_update_topology: rename + re-bridge.
        topology.name = "new-name".to_string();
        topology.updated_at = chrono::Utc::now();
        mgr.upsert_bridged_workflow(&topology, None)
            .await
            .expect("second upsert (after rename)");

        let rows = mgr.list_workflows(None).await.expect("list_workflows");
        assert_eq!(rows.len(), 1, "still one row after rename");
        assert_eq!(rows[0].title, "new-name");
    }

    /// Re-bridging a topology without an explicit project_id must preserve an
    /// already-bound project scope so the picker stays visible under a project
    /// filter after rename/update flows.
    #[tokio::test]
    async fn test_upsert_bridged_workflow_preserves_existing_project_id() {
        let (mgr, _dir) = manager();
        let project_id = Uuid::new_v4();
        let mut topology = topology_with(vec![node("a", SessionKind::Task)], vec![]);
        topology.name = "project-bound".to_string();

        let store = mgr.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .conn
                .execute(
                    "INSERT INTO projects (id, name, path, description, color, created_at, updated_at)
                     VALUES (?1, 'test-project', NULL, NULL, NULL, ?2, ?2)",
                    rusqlite::params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
                )
                .expect("project insert");
        })
        .await
        .expect("join");

        mgr.upsert_bridged_workflow(&topology, Some(project_id))
            .await
            .expect("initial project-bound upsert");

        topology.name = "project-bound-renamed".to_string();
        topology.updated_at = chrono::Utc::now();
        mgr.upsert_bridged_workflow(&topology, None)
            .await
            .expect("rename re-bridge should preserve project id");

        let workflow = mgr
            .get_workflow(derive_workflow_id(topology.id))
            .await
            .expect("get_workflow")
            .expect("workflow row should exist");
        assert_eq!(workflow.title, "project-bound-renamed");
        assert_eq!(workflow.project_id, Some(project_id));
    }

    /// Startup reconciliation backfills a missing workflow row for a stored
    /// topology so legacy/restored topologies appear in the gv picker.
    #[tokio::test]
    async fn test_reconcile_topology_workflows_backfills_legacy_topology_rows() {
        let (mgr, _dir) = manager();
        let topology = topology_with(vec![node("a", SessionKind::Task)], vec![]);

        let store = mgr.store.clone();
        let topology_clone = topology.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .insert_topology(&topology_clone)
                .expect("legacy topology insert");
        })
        .await
        .expect("join");

        let pre = mgr.list_workflows(None).await.expect("list_workflows pre");
        assert!(
            pre.is_empty(),
            "legacy topology fixture should start with no workflow rows"
        );

        let bridged = mgr
            .reconcile_topology_workflows()
            .await
            .expect("reconcile ok");
        assert_eq!(bridged, 1);

        let workflow = mgr
            .get_workflow(derive_workflow_id(topology.id))
            .await
            .expect("get_workflow")
            .expect("workflow row should be backfilled");
        assert_eq!(workflow.title, topology.name);
    }

    /// `delete_workflow_by_source_topology` cascades a delete: after the call,
    /// no rows remain mirrored from that topology id.
    #[tokio::test]
    async fn test_delete_topology_removes_workflow_row() {
        let (mgr, _dir) = manager();
        let topology = topology_with(vec![node("a", SessionKind::Task)], vec![]);
        mgr.upsert_bridged_workflow(&topology, None)
            .await
            .expect("upsert");

        let pre = mgr.list_workflows(None).await.expect("list_workflows pre");
        assert_eq!(pre.len(), 1);

        mgr.delete_workflow_by_source_topology(topology.id)
            .await
            .expect("delete");

        let post = mgr.list_workflows(None).await.expect("list_workflows post");
        assert_eq!(post.len(), 0, "row removed after cascade delete");
    }

    /// `delete_workflow_by_source_topology` is idempotent: deleting a topology
    /// that has no mirrored row succeeds without error.
    #[tokio::test]
    async fn test_delete_workflow_by_source_topology_is_idempotent() {
        let (mgr, _dir) = manager();
        let topology_id = Uuid::new_v4();
        // Row never inserted — call must still succeed.
        mgr.delete_workflow_by_source_topology(topology_id)
            .await
            .expect("delete on non-existent row should be idempotent");
    }

    /// `derive_workflow_id` is deterministic across calls and namespace-bound:
    /// the same topology id always derives to the same workflow id; different
    /// topology ids derive to different workflow ids.
    /// T3a-A3 (D1, #645): an author can never choose what a command node
    /// runs or how its effect is classified. argv, env, a script path, an
    /// `effect_class` field, an unknown op and a malformed filter are refused
    /// at upsert (`create_topology`), when a stored topology is bridged for
    /// `ExecuteTopology`, and when a snapshot reaches `execute_workflow_live`
    /// — each before any execution row exists.
    #[tokio::test]
    #[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
    async fn t3a_a3_author_command_input_rejected() {
        let (mgr, _dir) = manager();
        let mgr = Arc::new(mgr);
        let op = serde_json::json!({"name": "cargo_check_crate", "crate": "rsid"});
        let step = |op: serde_json::Value| serde_json::json!({"kind": "command", "op": op});
        let with_op = |key: &str, value: serde_json::Value| {
            let mut op = op.clone();
            op[key] = value;
            step(op)
        };
        let mut cases: Vec<(&str, HashMap<String, serde_json::Value>)> = vec![
            (
                "argv",
                HashMap::from([(
                    "step".into(),
                    with_op("argv", serde_json::json!(["sh", "-c", "x"])),
                )]),
            ),
            (
                "env",
                HashMap::from([("step".into(), with_op("env", serde_json::json!({"X": "1"})))]),
            ),
            (
                "script",
                HashMap::from([(
                    "step".into(),
                    with_op("script", serde_json::json!("/tmp/x.sh")),
                )]),
            ),
            (
                "unknown_op",
                HashMap::from([(
                    "step".into(),
                    step(serde_json::json!({"name": "rm_rf", "crate": "rsid"})),
                )]),
            ),
            (
                "bad_filter",
                HashMap::from([(
                    "step".into(),
                    step(
                        serde_json::json!({"name": "cargo_test_focused", "crate": "rsid", "filter": "a b;rm"}),
                    ),
                )]),
            ),
            (
                "bad_crate",
                HashMap::from([(
                    "step".into(),
                    step(serde_json::json!({"name": "cargo_clippy_crate", "crate": "--config=x"})),
                )]),
            ),
        ];
        let mut effect_class = step(op.clone());
        effect_class["effect_class"] = serde_json::json!("check");
        cases.push((
            "effect_class",
            HashMap::from([("step".into(), effect_class)]),
        ));
        for key in ["argv", "env", "script_path", "effect_class"] {
            cases.push((
                key,
                HashMap::from([
                    ("step".into(), step(op.clone())),
                    (key.into(), serde_json::json!("author supplied")),
                ]),
            ));
        }
        for (label, params) in cases {
            let def = TopologyDefinition {
                nodes: vec![node_with_params("c", SessionKind::Task, params.clone())],
                edges: vec![],
                until: None,
            };
            // Upsert.
            let refused = mgr
                .create_topology(format!("a3-{label}"), def.clone())
                .await
                .expect_err(label);
            assert!(
                matches!(refused, DaemonError::InvalidParam(_)),
                "{label}: {refused}"
            );
            // Execute: a stored row that bypassed upsert is refused by the bridge.
            let stored = topology_with(def.nodes.clone(), def.edges.clone());
            assert!(mgr.bridge_topology_to_workflow(&stored).is_err(), "{label}");
            // Execute: a snapshot authored directly carries the same input.
            let mut workflow = rsi_graph::format::WorkflowDefinition::new("a3");
            workflow = workflow.with_node(rsi_graph::format::NodeDef::action("c", "c"));
            if let Some(step) = params.get("step") {
                workflow.metadata.insert(
                    crate::topology::steps::STEPS_METADATA_KEY.into(),
                    GraphValue::String(serde_json::json!({ "c": step }).to_string()),
                );
            }
            for key in params.keys().filter(|key| key.as_str() != "step") {
                workflow.nodes[0]
                    .tags
                    .push(format!("{key}=author supplied"));
            }
            let refused = SessionManager::execute_workflow_live(
                Arc::clone(&mgr),
                Uuid::new_v4(),
                workflow,
                None,
                false,
                None,
                None,
                None,
            )
            .await
            .expect_err(label);
            assert!(
                matches!(refused, DaemonError::InvalidParam(_)),
                "{label}: {refused}"
            );
        }
        let executions: i64 = mgr
            .store()
            .lock()
            .await
            .conn
            .query_row("SELECT count(*) FROM topology_executions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(executions, 0);
        // The same op with typed params only is accepted.
        let accepted = TopologyDefinition {
            nodes: vec![node_with_params(
                "c",
                SessionKind::Task,
                HashMap::from([("step".into(), step(op))]),
            )],
            edges: vec![],
            until: None,
        };
        mgr.create_topology("a3-accepted".into(), accepted)
            .await
            .expect("typed catalog op is accepted");
    }

    #[test]
    fn derive_workflow_id_is_deterministic_and_distinct() {
        let topo1 = Uuid::new_v4();
        let topo2 = Uuid::new_v4();
        let id1a = derive_workflow_id(topo1);
        let id1b = derive_workflow_id(topo1);
        let id2 = derive_workflow_id(topo2);
        assert_eq!(id1a, id1b, "same topology id derives to same workflow id");
        assert_ne!(id1a, id2, "different topology ids derive to different ids");
    }
}
