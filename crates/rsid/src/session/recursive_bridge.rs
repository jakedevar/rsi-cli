//! Recursive task graph → `WorkflowDefinition` bridge (Phase-1, live).
//!
//! Pure, one-way translation layer: converts a recursive task graph
//! (`RecursiveTaskGraphSummary` + `&[RecursiveTaskNode]` + `&[RecursiveTaskEdge]`
//! + `&[RecursiveTaskAttempt]`) into a `WorkflowDefinition` consumable by the
//! `gv` graph renderer.
//!
//! Modeled exactly on `topology_bridge::bridge_topology_to_workflow`
//! (`topology_bridge.rs:111-366`) and kept as a separate file for single
//! responsibility (the topology bridge additionally owns `derive_workflow_id`
//! and the workflows-table upsert, which V0 has no analog for).
//!
//! # Invariants
//!
//! - `bridge_recursive_to_workflow` is **pure** (no async, no DB). The `&self`
//!   receiver mirrors the topology bridge so the two are discoverable as a pair;
//!   the body never reads `self`.
//! - The live caller is `rpc.rs` (`handle_get_recursive_graph_as_workflow`),
//!   gated on `gv_render_recursive_origin`.
//! - Node ids are preserved **verbatim** (`NodeDef::action(node.id.to_string(),
//!   …)`); the canonical UUID round-trips byte-for-byte. This is the load-bearing
//!   V0 invariant the V5 node→attempt join depends on.
//! - Both edge kinds (`ParentChild`, `Dependency`) are carried through as plain
//!   `EdgeDef`s; the distinction is preserved in the `recursive_edge_kinds`
//!   metadata sidecar (drawing them differently is V2+).

use rsi_common::recursive_dag::{
    RecursiveTaskAttempt, RecursiveTaskEdge, RecursiveTaskEdgeKind, RecursiveTaskGraphSummary,
    RecursiveTaskNode,
};
use rsi_graph::data::Value as GraphValue;
use rsi_graph::format::{EdgeDef, NodeDef, WorkflowDefinition};
use std::collections::BTreeMap;

use super::SessionManager;

impl SessionManager {
    /// Translate a recursive task graph into a `WorkflowDefinition`.
    ///
    /// This function is **pure** — it does not touch the database or any async
    /// runtime, and never reads `self`. The `&self` receiver mirrors
    /// `bridge_topology_to_workflow` so both bridges live on the same type.
    ///
    /// Infallible: the recursive input has no prereqs and no execution semantics
    /// in this render path, so there is no correctness condition to hard-fail on.
    /// Dangling-endpoint edges are carried through verbatim (the renderer
    /// tolerates and drops them).
    ///
    /// # Translation algorithm
    ///
    /// 1. **Nodes** (input order preserved): id verbatim; `objective →
    ///    instructions`; `depth → "depth:{n}"` tag; `scope → "scope:{}"` tag
    ///    (skipped when empty); each acceptance criterion → `"acceptance:{c}"`
    ///    tag (skipped when the list is empty).
    /// 2. **Edges**: all kinds carried as `EdgeDef::new(from, to)`; the kind is
    ///    recorded only in the `recursive_edge_kinds` sidecar.
    /// 3. **Metadata**: provenance stamps (`source_recursive_graph_id`,
    ///    `source_recursive_graph_title`, `bridged_at`), each `Some` origin
    ///    pointer, the always-present `recursive_edge_kinds` JSON sidecar, and
    ///    the always-present `recursive_node_locks` JSON sidecar (per-node
    ///    edit-lock state from the shared `recursive_node_lock` rule).
    #[allow(clippy::unused_self)]
    pub(crate) fn bridge_recursive_to_workflow(
        &self,
        summary: &RecursiveTaskGraphSummary,
        nodes: &[RecursiveTaskNode],
        edges: &[RecursiveTaskEdge],
        attempts: &[RecursiveTaskAttempt],
    ) -> WorkflowDefinition {
        // ── Node translation (input order preserved, id verbatim) ─────────
        let mut translated_nodes: Vec<NodeDef> = Vec::with_capacity(nodes.len());
        for node in nodes {
            let mut node_def = NodeDef::action(node.id.to_string(), node.title.clone());

            // objective → instructions (system-prompt equivalent).
            node_def.instructions.clone_from(&node.objective);

            // depth → tag (always).
            node_def.tags.push(format!("depth:{}", node.depth));

            // scope → tag (skip when empty).
            if !node.scope.is_empty() {
                node_def.tags.push(format!("scope:{}", node.scope));
            }

            // acceptance_criteria → tags (skip when the list is empty).
            for c in &node.acceptance_criteria {
                node_def.tags.push(format!("acceptance:{c}"));
            }

            translated_nodes.push(node_def);
        }

        // ── Edge translation (all kinds carried through) ──────────────────
        let translated_edges: Vec<EdgeDef> = edges
            .iter()
            .map(|e| EdgeDef::new(e.from_task_id.to_string(), e.to_task_id.to_string()))
            .collect();

        // ── Metadata assembly ─────────────────────────────────────────────
        let mut metadata: BTreeMap<String, GraphValue> = BTreeMap::new();

        metadata.insert(
            "source_recursive_graph_id".to_string(),
            GraphValue::String(summary.id.to_string()),
        );
        metadata.insert(
            "source_recursive_graph_title".to_string(),
            GraphValue::String(summary.title.clone()),
        );
        metadata.insert(
            "bridged_at".to_string(),
            GraphValue::String(chrono::Utc::now().to_rfc3339()),
        );

        // Existing origin pointers — stamped only when present.
        if let Some(workflow_id) = summary.workflow_id {
            metadata.insert(
                "workflow_id".to_string(),
                GraphValue::String(workflow_id.to_string()),
            );
        }
        if let Some(topology_id) = summary.topology_id {
            metadata.insert(
                "topology_id".to_string(),
                GraphValue::String(topology_id.to_string()),
            );
        }
        if let Some(parent_session_id) = summary.parent_session_id {
            metadata.insert(
                "parent_session_id".to_string(),
                GraphValue::String(parent_session_id.to_string()),
            );
        }
        if let Some(source_execution_id) = &summary.source_execution_id {
            metadata.insert(
                "source_execution_id".to_string(),
                GraphValue::String(source_execution_id.clone()),
            );
        }
        if let Some(source_eval_id) = &summary.source_eval_id {
            metadata.insert(
                "source_eval_id".to_string(),
                GraphValue::String(source_eval_id.clone()),
            );
        }

        // recursive_edge_kinds — the V0 sidecar. Always stamped (even for empty
        // edges, yielding "[]") so the dialect is always present for the
        // renderer. The kind is written as the bare snake_case wire string via
        // an explicit match so the JSON-object value is `"parent_child"` rather
        // than a JSON-encoded `"\"parent_child\""`.
        let edge_kind_json: Vec<serde_json::Value> = edges
            .iter()
            .map(|e| {
                serde_json::json!({
                    "from": e.from_task_id.to_string(),
                    "to": e.to_task_id.to_string(),
                    "kind": match e.kind {
                        RecursiveTaskEdgeKind::ParentChild => "parent_child",
                        RecursiveTaskEdgeKind::Dependency => "dependency",
                    },
                })
            })
            .collect();
        metadata.insert(
            "recursive_edge_kinds".to_string(),
            GraphValue::String(serde_json::to_string(&edge_kind_json).unwrap_or_default()),
        );

        // recursive_node_locks — the V2 two-way sidecar (task_id → {locked,
        // reason}). Computed by the SHARED `recursive_node_lock` helper, the SAME
        // rule the server-side `ensure_recursive_node_editable_tx` precondition
        // uses, so the gv per-node lock hint matches the server's accept/reject
        // decision exactly. gv derives per-node edit-lock state from this sidecar
        // with no extra RPC.
        let node_lock_json: serde_json::Map<String, serde_json::Value> = nodes
            .iter()
            .map(|node| {
                let attempts_for_task: Vec<&RecursiveTaskAttempt> = attempts
                    .iter()
                    .filter(|attempt| attempt.task_id == node.id)
                    .collect();
                let attempts_owned: Vec<RecursiveTaskAttempt> =
                    attempts_for_task.into_iter().cloned().collect();
                let (locked, reason) =
                    crate::store::recursive_dag::recursive_node_lock(node.status, &attempts_owned);
                (
                    node.id.to_string(),
                    serde_json::json!({
                        "locked": locked,
                        "reason": reason.unwrap_or_default(),
                    }),
                )
            })
            .collect();
        metadata.insert(
            "recursive_node_locks".to_string(),
            GraphValue::String(
                serde_json::to_string(&serde_json::Value::Object(node_lock_json))
                    .unwrap_or_default(),
            ),
        );

        let node_strategy_json: serde_json::Map<String, serde_json::Value> = nodes
            .iter()
            .map(|node| {
                (
                    node.id.to_string(),
                    serde_json::json!({
                        "integration_strategy": node.integration_strategy,
                        "verification_strategy": node.verification_strategy,
                    }),
                )
            })
            .collect();
        metadata.insert(
            "recursive_node_strategies".to_string(),
            GraphValue::String(
                serde_json::to_string(&serde_json::Value::Object(node_strategy_json))
                    .unwrap_or_default(),
            ),
        );

        WorkflowDefinition {
            version: "1.0".to_string(),
            name: summary.title.clone(),
            description: String::new(),
            nodes: translated_nodes,
            edges: translated_edges,
            metadata,
        }
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
    use rsi_common::recursive_dag::{
        RecursiveAttemptId, RecursiveAttemptPhase, RecursiveExecutionMode, RecursiveGraphStatus,
        RecursiveTaskGraphId, RecursiveTaskId, RecursiveTaskLifecycleState,
    };
    use std::collections::HashSet;
    use std::sync::Arc;
    use tempfile::TempDir;
    use uuid::Uuid;

    // ── Local test helpers ────────────────────────────────────────────────

    /// Build a minimal `SessionManager` backed by a temp `SQLite` store.
    /// The returned `TempDir` MUST be bound in the caller to avoid early drop.
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

    /// Build a recursive task node with every field populated.
    #[allow(clippy::too_many_arguments)]
    fn rec_node(
        id: RecursiveTaskId,
        graph_id: RecursiveTaskGraphId,
        parent_task_id: Option<RecursiveTaskId>,
        title: &str,
        objective: &str,
        scope: &str,
        accept: &[&str],
        depth: u32,
    ) -> RecursiveTaskNode {
        let now = Utc::now();
        RecursiveTaskNode {
            id,
            graph_id,
            parent_task_id,
            title: title.to_string(),
            objective: objective.to_string(),
            scope: scope.to_string(),
            acceptance_criteria: accept.iter().map(ToString::to_string).collect(),
            depth,
            scope_units: 1,
            max_retries: 0,
            status: RecursiveTaskLifecycleState::Planning,
            decomposed_once: false,
            integration_strategy: None,
            verification_strategy: None,
            blocked_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Build a recursive task edge with every field populated.
    fn rec_edge(
        id: i64,
        graph_id: RecursiveTaskGraphId,
        from: RecursiveTaskId,
        to: RecursiveTaskId,
        kind: RecursiveTaskEdgeKind,
    ) -> RecursiveTaskEdge {
        RecursiveTaskEdge {
            id,
            graph_id,
            from_task_id: from,
            to_task_id: to,
            kind,
            injection_batch_id: None,
            created_at: Utc::now(),
        }
    }

    /// Build a graph summary with every field populated. `workflow_id` is set
    /// (to exercise the conditional origin stamp); `topology_id` and the other
    /// pointers stay `None` (to lock the conditional absence).
    fn summary(
        graph_id: RecursiveTaskGraphId,
        root_id: RecursiveTaskId,
        title: &str,
    ) -> RecursiveTaskGraphSummary {
        let now = Utc::now();
        RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id: root_id,
            title: title.to_string(),
            objective: "graph objective".to_string(),
            status: RecursiveGraphStatus::Active,
            project_id: None,
            workflow_id: Some(Uuid::new_v4()),
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            execution_mode: RecursiveExecutionMode::Fake,
            max_depth: 4,
            max_fanout: 4,
            max_descendants: 16,
            step_limit: 100,
            last_stop_reason: None,
            malformed_reason: None,
            created_at: now,
            updated_at: now,
            recovered_at: None,
            quarantined_at: None,
            quarantine_reason: None,
            recovery_checked_at: None,
        }
    }

    /// Root + 2 children; 2 `ParentChild` edges + 1 `Dependency` edge.
    /// Returns `(summary, nodes, edges, root_id, child_a_id, child_b_id)`.
    #[allow(clippy::type_complexity)]
    fn fixture() -> (
        RecursiveTaskGraphSummary,
        Vec<RecursiveTaskNode>,
        Vec<RecursiveTaskEdge>,
        RecursiveTaskId,
        RecursiveTaskId,
        RecursiveTaskId,
    ) {
        let graph_id = RecursiveTaskGraphId::new();
        let root_id = RecursiveTaskId::new();
        let child_a_id = RecursiveTaskId::new();
        let child_b_id = RecursiveTaskId::new();

        let nodes = vec![
            rec_node(
                root_id,
                graph_id,
                None,
                "root",
                "root objective",
                "",
                &[],
                0,
            ),
            rec_node(
                child_a_id,
                graph_id,
                Some(root_id),
                "childA",
                "child A objective",
                "module-a",
                &["compiles", "tested"],
                1,
            ),
            rec_node(
                child_b_id,
                graph_id,
                Some(root_id),
                "childB",
                "child B objective",
                "module-b",
                &["compiles"],
                1,
            ),
        ];

        let edges = vec![
            rec_edge(
                1,
                graph_id,
                root_id,
                child_a_id,
                RecursiveTaskEdgeKind::ParentChild,
            ),
            rec_edge(
                2,
                graph_id,
                root_id,
                child_b_id,
                RecursiveTaskEdgeKind::ParentChild,
            ),
            rec_edge(
                3,
                graph_id,
                child_a_id,
                child_b_id,
                RecursiveTaskEdgeKind::Dependency,
            ),
        ];

        let sm = summary(graph_id, root_id, "fixture graph");
        (sm, nodes, edges, root_id, child_a_id, child_b_id)
    }

    // ── Structural preconditions for layout_workflow_graph ─────────────────

    /// Assertion 1: ≥1 node and node count == input count (no drop, no synthesis).
    #[tokio::test]
    async fn test_bridge_node_count_matches_input() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, ..) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);
        assert_eq!(wf.nodes.len(), 3);
        assert_eq!(wf.edges.len(), 3);
    }

    /// Assertion 2: node ids are unique (layout keys `node_index_by_id` on id).
    #[tokio::test]
    async fn test_bridge_node_ids_unique() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, ..) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);
        let ids: Vec<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
        let set: HashSet<&&str> = ids.iter().collect();
        assert_eq!(ids.len(), set.len(), "node ids must be unique: {ids:?}");
    }

    /// Assertion 3: every edge endpoint references an existing node id
    /// (layout silently drops dangling endpoints).
    #[tokio::test]
    async fn test_bridge_edge_endpoints_reference_existing_nodes() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, ..) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);
        let node_ids: HashSet<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
        for e in &wf.edges {
            assert!(
                node_ids.contains(e.source.as_str()),
                "edge source {} missing from nodes",
                e.source
            );
            assert!(
                node_ids.contains(e.target.as_str()),
                "edge target {} missing from nodes",
                e.target
            );
        }
        assert_eq!(wf.edges.len(), 3, "all three edges must be carried through");
    }

    /// Assertion 4: `NodeDef.id` == `RecursiveTaskId` verbatim (load-bearing V0
    /// invariant; the V5 node→attempt join depends on it).
    #[tokio::test]
    async fn test_bridge_node_id_verbatim() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, root_id, child_a_id, child_b_id) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);
        assert_eq!(wf.nodes[0].id, root_id.to_string());
        assert_eq!(wf.nodes[1].id, child_a_id.to_string());
        assert_eq!(wf.nodes[2].id, child_b_id.to_string());
        // name carries the title.
        assert_eq!(wf.nodes[0].name, "root");
        // top-level workflow name is the graph title.
        assert_eq!(wf.name, "fixture graph");
    }

    // ── Metadata sidecar + provenance ──────────────────────────────────────

    /// Assertion 5: `recursive_edge_kinds` sidecar with `snake_case` bare-string kinds.
    #[tokio::test]
    async fn test_bridge_recursive_edge_kinds_sidecar() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, ..) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);
        assert!(wf.metadata.contains_key("recursive_edge_kinds"));
        let GraphValue::String(json) = &wf.metadata["recursive_edge_kinds"] else {
            panic!("recursive_edge_kinds must be a String value");
        };
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(json).expect("recursive_edge_kinds must be valid JSON");
        assert_eq!(parsed.len(), 3);
        let dependency_count = parsed.iter().filter(|e| e["kind"] == "dependency").count();
        let parent_child_count = parsed
            .iter()
            .filter(|e| e["kind"] == "parent_child")
            .count();
        assert_eq!(dependency_count, 1, "one dependency edge expected");
        assert_eq!(parent_child_count, 2, "two parent_child edges expected");
    }

    /// `recursive_edge_kinds` is always present, even for an empty edge set ("[]").
    #[tokio::test]
    async fn test_bridge_recursive_edge_kinds_always_present_when_empty() {
        let (mgr, _dir) = manager();
        let graph_id = RecursiveTaskGraphId::new();
        let root_id = RecursiveTaskId::new();
        let nodes = vec![rec_node(root_id, graph_id, None, "root", "obj", "", &[], 0)];
        let sm = summary(graph_id, root_id, "solo");
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &[], &[]);
        let GraphValue::String(json) = &wf.metadata["recursive_edge_kinds"] else {
            panic!("recursive_edge_kinds must be a String value");
        };
        assert_eq!(json, "[]");
    }

    /// Assertion 6: provenance stamps + conditional origin pointers.
    #[tokio::test]
    async fn test_bridge_provenance_and_conditional_origin_stamps() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, ..) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);
        assert!(wf.metadata.contains_key("source_recursive_graph_id"));
        assert!(wf.metadata.contains_key("source_recursive_graph_title"));
        assert!(wf.metadata.contains_key("bridged_at"));
        // fixture set workflow_id: Some(..) → stamped.
        assert!(wf.metadata.contains_key("workflow_id"));
        // fixture left these None → NOT stamped (locks the conditional stamp).
        assert!(!wf.metadata.contains_key("topology_id"));
        assert!(!wf.metadata.contains_key("parent_session_id"));
        assert!(!wf.metadata.contains_key("source_execution_id"));
        assert!(!wf.metadata.contains_key("source_eval_id"));
    }

    /// Assertion 7: per-node depth / scope / acceptance tags.
    #[tokio::test]
    async fn test_bridge_node_tags() {
        let (mgr, _dir) = manager();
        let (sm, nodes, edges, ..) = fixture();
        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &[]);

        // root: depth 0, empty scope (no scope tag), no acceptance tags.
        assert!(wf.nodes[0].tags.iter().any(|t| t == "depth:0"));
        assert!(!wf.nodes[0].tags.iter().any(|t| t.starts_with("scope:")));
        assert!(
            !wf.nodes[0]
                .tags
                .iter()
                .any(|t| t.starts_with("acceptance:"))
        );

        // childA: depth 1, scope:module-a, acceptance:compiles + acceptance:tested.
        assert!(wf.nodes[1].tags.iter().any(|t| t == "depth:1"));
        assert!(wf.nodes[1].tags.iter().any(|t| t == "scope:module-a"));
        assert!(wf.nodes[1].tags.iter().any(|t| t == "acceptance:compiles"));
        assert!(wf.nodes[1].tags.iter().any(|t| t == "acceptance:tested"));

        // objective → instructions.
        assert_eq!(wf.nodes[1].instructions, "child A objective");
    }

    /// Assertion e: bridge stamps recursive_node_locks correctly.
    #[tokio::test]
    async fn test_bridge_stamps_recursive_node_locks() {
        let (mgr, _dir) = manager();
        let (sm, mut nodes, edges, root_id, child_a_id, child_b_id) = fixture();

        // Let's modify nodes to have different status values.
        nodes[0].status = RecursiveTaskLifecycleState::Planning;
        nodes[1].status = RecursiveTaskLifecycleState::Ready;
        nodes[2].status = RecursiveTaskLifecycleState::Pending;

        use rsi_common::recursive_dag::RecursiveAttemptStatus;
        let attempts = vec![RecursiveTaskAttempt {
            id: RecursiveAttemptId::new(),
            graph_id: sm.id,
            task_id: child_a_id,
            phase: RecursiveAttemptPhase::Execute,
            attempt_no: 1,
            retry_count: 0,
            status: RecursiveAttemptStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            failure_reason: None,
            block_reason: None,
            dependency_snapshot: Vec::new(),
            executor_kind: RecursiveExecutionMode::Fake,
            session_id: None,
            workflow_execution_id: None,
        }];

        let wf = mgr.bridge_recursive_to_workflow(&sm, &nodes, &edges, &attempts);
        assert!(wf.metadata.contains_key("recursive_node_locks"));
        let GraphValue::String(json) = &wf.metadata["recursive_node_locks"] else {
            panic!("recursive_node_locks must be a String value");
        };
        let parsed: serde_json::Value =
            serde_json::from_str(json).expect("recursive_node_locks must be valid JSON");

        // root (Planning) -> locked
        assert!(parsed[&root_id.to_string()]["locked"].as_bool().unwrap());
        assert!(
            parsed[&root_id.to_string()]["reason"]
                .as_str()
                .unwrap()
                .contains("planning")
        );

        // childA (Ready but has Running attempt) -> locked
        assert!(parsed[&child_a_id.to_string()]["locked"].as_bool().unwrap());
        assert!(
            parsed[&child_a_id.to_string()]["reason"]
                .as_str()
                .unwrap()
                .contains("attempt running")
        );

        // childB (Pending, no attempts) -> unlocked
        assert!(!parsed[&child_b_id.to_string()]["locked"].as_bool().unwrap());

        // Check recursive_node_strategies
        assert!(wf.metadata.contains_key("recursive_node_strategies"));
        let GraphValue::String(strat_json) = &wf.metadata["recursive_node_strategies"] else {
            panic!("recursive_node_strategies must be a String value");
        };
        let strat_parsed: serde_json::Value =
            serde_json::from_str(strat_json).expect("recursive_node_strategies must be valid JSON");
        assert_eq!(
            strat_parsed[&root_id.to_string()]["integration_strategy"],
            serde_json::Value::Null
        );
    }
}
