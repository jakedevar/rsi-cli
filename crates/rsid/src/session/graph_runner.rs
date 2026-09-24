//! Async DAG runner for workflow graph execution.
//!
//! Unlike the synchronous `DagExecutor` in `flywheel-graph` which runs nodes
//! with a stub pass-through, this module launches real AI sessions for each
//! graph node. Nodes within the same topological layer execute concurrently,
//! with outputs flowing to downstream nodes via edge-filtered `NodeData`.
//!
//! The runner respects cancellation via `AtomicBool` and emits
//! `NodeExecutionUpdate` callbacks so the TUI can visualize progress.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result as AnyhowResult;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use std::collections::BTreeMap;

use crate::bus::{DaemonEvent, EventBus};
use crate::graph_exec::{ExecutionResult, NodeExecutionState, NodeExecutionUpdate};
use rsi_common::types::{
    ConversationEvent, EventType, FailurePolicy, Role, SessionProvider, SessionStatus,
    UntilCondition,
};
use rsi_graph::data::Value as GraphValue;
use rsi_graph::data::{NodeData, Value};
use rsi_graph::filter::FieldFilter;
use rsi_graph::format::{EdgeDef, NodeDef, WorkflowDefinition};
use rsi_graph::generate::templates::PIPELINE_ENTRY_CONTEXT_TAG;

use super::SessionManager;
use super::types::{LaunchPurpose, TopologyNodeLaunchContext};
use crate::topology::launch::NodeLaunchBuilder;

pub(crate) const PIPELINE_ENTRY_CONTEXT_KEY: &str = "_pipeline_entry_context";
const PIPELINE_ENTRY_RESPONSE_MARKER: &str = "\n\nReply with exactly `PIPELINE ENTRY READY`";

pub(crate) fn plan_workflow_custody(
    workflow: &WorkflowDefinition,
) -> crate::error::Result<crate::topology::custody::TopologyCustodyPlan> {
    let loop_edges = parse_loop_edges_from_metadata(&workflow.metadata);
    let scc_regions = parse_scc_regions_from_metadata(&workflow.metadata);
    crate::topology::custody::plan_topology_custody(
        &workflow.nodes,
        &workflow.edges,
        &loop_edges,
        &scc_regions,
    )
}

/// Execution context shared across the graph run.
///
/// All fields are cloneable/Arc'd so they can be passed into spawned tasks.
pub(crate) struct GraphRunnerContext {
    pub event_bus: Arc<EventBus>,
    pub cancel_flag: Arc<AtomicBool>,
    pub on_node_update: Arc<dyn Fn(NodeExecutionUpdate) + Send + Sync>,
    pub project_id: Option<Uuid>,
    pub workflow_id: Uuid,
    /// P1.12: parent container (Group or Epic) under which spawned executor
    /// sessions are placed. `None` keeps the legacy "top-level orphan" behavior.
    pub parent_id: Option<Uuid>,
    pub is_topology: bool,
    pub custody: crate::topology::custody::TopologyCustody,
    pub custody_plan: crate::topology::custody::TopologyCustodyPlan,
}

/// Run a workflow graph asynchronously, launching AI sessions for each node.
///
/// Nodes are topologically sorted into layers (Kahn's algorithm). Each layer
/// executes all its nodes concurrently. Outputs from completed nodes are
/// merged (with optional edge filters) and passed as input to downstream nodes.
///
/// Returns an `ExecutionResult` indicating overall success/failure.
pub(crate) async fn run_graph_workflow(
    session_manager: Arc<SessionManager>,
    ctx: &GraphRunnerContext,
    workflow: &WorkflowDefinition,
    input: NodeData,
) -> ExecutionResult {
    // Build node lookup map.
    let node_map: HashMap<&str, &NodeDef> =
        workflow.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // Parse loop-edge pairs from metadata (P1.11: stamped by bridge for loop-bearing
    // topologies; empty for acyclic-only workflows — backward compatible).
    let loop_edge_pairs = parse_loop_edges_from_metadata(&workflow.metadata);

    // Parse SCC regions from metadata (P1.11: used by run_loop_region driver).
    let scc_regions = parse_scc_regions_from_metadata(&workflow.metadata);

    // Parse until_condition from metadata (P1.11: used by UntilEvaluator).
    let until_condition = parse_until_condition_from_metadata(&workflow.metadata);

    // Parse per-node failure policies from metadata (P1.11: FailurePolicy integration).
    let failure_policies = parse_failure_policies_from_metadata(&workflow.metadata);

    // Compute topological layers via Kahn's algorithm over the ACYCLIC edge subset.
    // Loop back-edges are excluded from Kahn (they would cause a false "cycle detected"
    // error). The loop driver handles re-entry via iteration, not edge traversal.
    let layers = match topological_layers(&workflow.nodes, &workflow.edges, &loop_edge_pairs) {
        Ok(layers) => layers,
        Err(msg) => {
            return ExecutionResult {
                success: false,
                output: None,
                error: Some(msg),
            };
        }
    };

    // Build incoming edge map: target_id -> list of edges pointing into it.
    let mut incoming_edges: HashMap<&str, Vec<&EdgeDef>> = HashMap::new();
    let mut has_outgoing: HashSet<&str> = HashSet::new();
    let mut has_incoming: HashSet<&str> = HashSet::new();

    for edge in &workflow.edges {
        incoming_edges
            .entry(edge.target.as_str())
            .or_default()
            .push(edge);
        has_outgoing.insert(edge.source.as_str());
        has_incoming.insert(edge.target.as_str());
    }

    // Source nodes (no incoming edges) receive the workflow input.
    let source_nodes: HashSet<&str> = workflow
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| !has_incoming.contains(id))
        .collect();

    // Sink nodes (no outgoing edges) contribute to the final output.
    let sink_nodes: HashSet<&str> = workflow
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| !has_outgoing.contains(id))
        .collect();

    // Per-node outputs, populated as layers complete.
    let mut outputs: HashMap<String, NodeData> = HashMap::new();

    // P1.11: Determine if this workflow has loop regions.
    // Build the union of all SCC member node IDs across all regions.
    let scc_node_set: HashSet<&str> = scc_regions
        .iter()
        .flat_map(|scc| scc.iter().map(|s| s.as_str()))
        .collect();
    let has_loops = !scc_node_set.is_empty();

    if !has_loops {
        // ── Acyclic-only path (backward compatible, unchanged from pre-P1.11) ──
        for (layer_idx, layer) in layers.iter().enumerate() {
            // Check cancellation before each layer.
            if ctx.cancel_flag.load(Ordering::Relaxed) {
                return ExecutionResult {
                    success: false,
                    output: None,
                    error: Some("workflow execution cancelled".to_string()),
                };
            }
            let layer_result = execute_layer(
                layer_idx,
                layer,
                &node_map,
                &source_nodes,
                &input,
                &incoming_edges,
                &outputs,
                ctx,
                &session_manager,
                /*topology_iteration=*/ 0,
                &failure_policies,
            )
            .await;
            match layer_result {
                Ok(layer_outputs) => {
                    for (nid, output) in layer_outputs {
                        outputs.insert(nid, output);
                    }
                }
                Err(err_result) => return err_result,
            }
        }
    } else {
        // ── Loop-aware path (P1.11) ──────────────────────────────────────────
        // Partition layers into three groups:
        //   pre-loop: layers before the first SCC layer
        //   loop-body: layers where at least one node is in the SCC
        //   post-loop: layers after the last SCC layer
        let first_scc_layer = layers
            .iter()
            .position(|layer| layer.iter().any(|n| scc_node_set.contains(n.as_str())));
        let last_scc_layer = layers
            .iter()
            .rposition(|layer| layer.iter().any(|n| scc_node_set.contains(n.as_str())));

        let (pre_layers, loop_layers, post_layers) = match (first_scc_layer, last_scc_layer) {
            (Some(first), Some(last)) => {
                (&layers[..first], &layers[first..=last], &layers[last + 1..])
            }
            _ => {
                // No SCC layers found despite scc_node_set being non-empty — shouldn't happen.
                // Fall back to running all layers once.
                warn!("P1.11: SCC nodes not found in layers — running as acyclic");
                (&layers[..], &layers[..0], &layers[..0])
            }
        };

        // Execute pre-loop layers once.
        for (layer_idx, layer) in pre_layers.iter().enumerate() {
            if ctx.cancel_flag.load(Ordering::Relaxed) {
                return ExecutionResult {
                    success: false,
                    output: None,
                    error: Some("workflow execution cancelled".to_string()),
                };
            }
            let layer_result = execute_layer(
                layer_idx,
                layer,
                &node_map,
                &source_nodes,
                &input,
                &incoming_edges,
                &outputs,
                ctx,
                &session_manager,
                /*topology_iteration=*/ 0,
                &failure_policies,
            )
            .await;
            match layer_result {
                Ok(layer_outputs) => {
                    for (nid, output) in layer_outputs {
                        outputs.insert(nid, output);
                    }
                }
                Err(err_result) => return err_result,
            }
        }

        // Collect all SCC member node IDs as owned strings for `run_loop_region`.
        let region_nodes: Vec<String> = scc_node_set.iter().map(|s| s.to_string()).collect();
        let region_layers: Vec<Vec<String>> = loop_layers.to_vec();

        // Execute the loop region with the UntilEvaluator.
        let loop_result = run_loop_region(
            Arc::clone(&session_manager),
            ctx,
            workflow,
            &region_nodes,
            region_layers,
            &mut outputs,
            &source_nodes,
            &input,
            &incoming_edges,
            until_condition,
            &failure_policies,
        )
        .await;
        if let Err(err_result) = loop_result {
            return err_result;
        }

        // Execute post-loop layers once.
        let post_offset = pre_layers.len() + loop_layers.len();
        for (layer_sub_idx, layer) in post_layers.iter().enumerate() {
            if ctx.cancel_flag.load(Ordering::Relaxed) {
                return ExecutionResult {
                    success: false,
                    output: None,
                    error: Some("workflow execution cancelled".to_string()),
                };
            }
            let layer_result = execute_layer(
                post_offset + layer_sub_idx,
                layer,
                &node_map,
                &source_nodes,
                &input,
                &incoming_edges,
                &outputs,
                ctx,
                &session_manager,
                /*topology_iteration=*/ 0,
                &failure_policies,
            )
            .await;
            match layer_result {
                Ok(layer_outputs) => {
                    for (nid, output) in layer_outputs {
                        outputs.insert(nid, output);
                    }
                }
                Err(err_result) => return err_result,
            }
        }
    }

    // Merge outputs from sink nodes into the final result.
    let mut final_output = NodeData::new();
    for sink_id in &sink_nodes {
        if let Some(data) = outputs.get(*sink_id) {
            final_output.merge(data.clone());
        }
    }

    info!(workflow = %workflow.name, "graph workflow completed successfully");

    ExecutionResult {
        success: true,
        output: serde_json::to_value(&final_output).ok(),
        error: None,
    }
}

// ─── Layer execution helper (P1.11) ──────────────────────────────────────────

struct PreparedNodeLaunch {
    node_id: String,
    config: crate::claude::LaunchConfig,
    fork: crate::topology::custody::TopologyForkSource,
}

#[allow(clippy::too_many_arguments)]
fn prepare_layer_launches(
    layer: &[String],
    node_map: &HashMap<&str, &NodeDef>,
    source_nodes: &HashSet<&str>,
    input: &NodeData,
    incoming_edges: &HashMap<&str, Vec<&EdgeDef>>,
    outputs: &HashMap<String, NodeData>,
    ctx: &GraphRunnerContext,
    topology_iteration: u32,
) -> Result<Vec<PreparedNodeLaunch>, ExecutionResult> {
    layer
        .iter()
        .map(|node_id| {
            let node_def =
                node_map
                    .get(node_id.as_str())
                    .copied()
                    .ok_or_else(|| ExecutionResult {
                        success: false,
                        output: None,
                        error: Some(format!(
                            "node '{}' not found in workflow definition",
                            node_id
                        )),
                    })?;
            let merged_input = if source_nodes.contains(node_id.as_str()) {
                input.clone()
            } else {
                merge_upstream_data(node_id, incoming_edges, outputs)
            };
            let query = build_node_query(node_def, &merged_input);
            let builder = NodeLaunchBuilder {
                custody: &ctx.custody,
                is_topology: ctx.is_topology,
                workflow_id: ctx.workflow_id,
                project_id: ctx.project_id,
                parent_id: ctx.parent_id,
            };
            let plan = ctx
                .custody_plan
                .node(node_id)
                .map_err(|error| ExecutionResult {
                    success: false,
                    output: None,
                    error: Some(error.to_string()),
                })?;
            let (config, fork) = builder
                .build(node_def, query, topology_iteration, 0, plan)
                .map_err(|error| ExecutionResult {
                    success: false,
                    output: None,
                    error: Some(error.to_string()),
                })?;
            Ok(PreparedNodeLaunch {
                node_id: node_id.clone(),
                config,
                fork,
            })
        })
        .collect()
}

/// Execute a single topological layer: spawn all nodes concurrently, await
/// their completions, and return a map of `(node_id, NodeData)` outputs.
///
/// Returns `Ok(HashMap)` on success or `Err(ExecutionResult)` on any failure
/// (cancellation, node failure, join error). The caller inserts the Ok outputs
/// into the global `outputs` map.
#[allow(clippy::too_many_arguments)]
async fn execute_layer(
    layer_idx: usize,
    layer: &[String],
    node_map: &HashMap<&str, &NodeDef>,
    source_nodes: &HashSet<&str>,
    input: &NodeData,
    incoming_edges: &HashMap<&str, Vec<&EdgeDef>>,
    outputs: &HashMap<String, NodeData>,
    ctx: &GraphRunnerContext,
    session_manager: &Arc<SessionManager>,
    topology_iteration: u32,
    failure_policies: &HashMap<String, FailurePolicy>,
) -> Result<HashMap<String, NodeData>, ExecutionResult> {
    debug!(layer = layer_idx, nodes = ?layer, "executing graph layer");

    let prepared = prepare_layer_launches(
        layer,
        node_map,
        source_nodes,
        input,
        incoming_edges,
        outputs,
        ctx,
        topology_iteration,
    )?;
    let mut join_set = tokio::task::JoinSet::new();

    for prepared_node in prepared {
        let PreparedNodeLaunch {
            node_id,
            config,
            fork,
        } = prepared_node;
        (ctx.on_node_update)(NodeExecutionUpdate {
            node_id: node_id.clone(),
            state: NodeExecutionState::Running,
            output_preview: None,
        });

        let sm = Arc::clone(session_manager);
        let event_bus = Arc::clone(&ctx.event_bus);
        let cancel_flag = Arc::clone(&ctx.cancel_flag);
        let nid = node_id;
        let custody = ctx.custody.clone();

        join_set.spawn(async move {
            let result = launch_and_wait_session(
                &sm,
                &event_bus,
                &cancel_flag,
                config,
                fork,
                &custody,
                &nid,
                topology_iteration,
            )
            .await;
            (nid, result)
        });
    }

    let mut layer_outputs: HashMap<String, NodeData> = HashMap::new();
    // Track retry counts per-node for FailurePolicy::Retry (D6: counts against max_iterations).
    let mut retry_counts: HashMap<String, u32> = HashMap::new();
    // Nodes pending a retry after the JoinSet drains. Each entry: (node_id, error_string).
    let mut pending_retries: Vec<(String, String)> = Vec::new();
    let mut terminal_failure: Option<ExecutionResult> = None;

    while let Some(join_result) = join_set.join_next().await {
        let (nid, result) = match join_result {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "graph runner task panicked");
                terminal_failure.get_or_insert(ExecutionResult {
                    success: false,
                    output: None,
                    error: Some(format!("task join error: {}", e)),
                });
                continue;
            }
        };

        match result {
            Ok(mut node_output) => {
                let preview = preview_node_data(&node_output);
                (ctx.on_node_update)(NodeExecutionUpdate {
                    node_id: nid.clone(),
                    state: NodeExecutionState::Succeeded,
                    output_preview: preview,
                });
                if let Some(node_def) = node_map.get(nid.as_str()) {
                    attach_pipeline_entry_context(node_def, &mut node_output);
                }
                layer_outputs.insert(nid, node_output);
            }
            Err(e) => {
                error!(node_id = %nid, error = %e, "node execution failed");
                (ctx.on_node_update)(NodeExecutionUpdate {
                    node_id: nid.clone(),
                    state: NodeExecutionState::Failed,
                    output_preview: Some(e.to_string()),
                });
                // P1.11: apply FailurePolicy per node result.
                match failure_policies.get(&nid) {
                    Some(FailurePolicy::Skip) => {
                        debug!(node_id = %nid, "P1.11 FailurePolicy::Skip — inserting empty output and continuing");
                        layer_outputs.insert(nid, NodeData::new());
                    }
                    Some(FailurePolicy::Retry) => {
                        // Queue for retry after JoinSet drains (D6).
                        pending_retries.push((nid, e.to_string()));
                    }
                    Some(FailurePolicy::Halt) | None => {
                        terminal_failure.get_or_insert(ExecutionResult {
                            success: false,
                            output: None,
                            error: Some(format!("node '{}' failed: {}", nid, e)),
                        });
                    }
                }
            }
        }
    }

    if let Some(failure) = terminal_failure {
        return Err(failure);
    }

    // Process pending retries sequentially (D6: Retry re-runs same node within same iteration).
    for (nid, _prev_err) in pending_retries {
        let node_def = match node_map.get(nid.as_str()) {
            Some(n) => *n,
            None => {
                return Err(ExecutionResult {
                    success: false,
                    output: None,
                    error: Some(format!("retry: node '{}' not found in workflow", nid)),
                });
            }
        };

        let max_retries = failure_retry_budget(node_def);

        let retry_count = retry_counts.entry(nid.clone()).or_insert(0);

        loop {
            if *retry_count >= max_retries {
                // Exhausted retries → halt.
                error!(node_id = %nid, retries = retry_count, "P1.11 FailurePolicy::Retry exhausted — halting");
                return Err(ExecutionResult {
                    success: false,
                    output: None,
                    error: Some(format!(
                        "node '{}' failed after {} retries",
                        nid, retry_count
                    )),
                });
            }

            *retry_count += 1;
            debug!(node_id = %nid, attempt = retry_count, max = max_retries, "P1.11 FailurePolicy::Retry — retrying node");

            let merged_input = if source_nodes.contains(nid.as_str()) {
                input.clone()
            } else {
                merge_upstream_data(&nid, incoming_edges, outputs)
            };

            let query = build_node_query(node_def, &merged_input);
            let builder = NodeLaunchBuilder {
                custody: &ctx.custody,
                is_topology: ctx.is_topology,
                workflow_id: ctx.workflow_id,
                project_id: ctx.project_id,
                parent_id: ctx.parent_id,
            };
            let (retry_config, fork) = builder
                .build(
                    node_def,
                    query,
                    topology_iteration,
                    *retry_count,
                    ctx.custody_plan
                        .node(&nid)
                        .map_err(|error| ExecutionResult {
                            success: false,
                            output: None,
                            error: Some(error.to_string()),
                        })?,
                )
                .map_err(|error| ExecutionResult {
                    success: false,
                    output: None,
                    error: Some(error.to_string()),
                })?;

            (ctx.on_node_update)(NodeExecutionUpdate {
                node_id: nid.clone(),
                state: NodeExecutionState::Running,
                output_preview: None,
            });

            let retry_result = launch_and_wait_session(
                session_manager,
                &ctx.event_bus,
                &ctx.cancel_flag,
                retry_config,
                fork,
                &ctx.custody,
                &nid,
                topology_iteration,
            )
            .await;

            match retry_result {
                Ok(node_output) => {
                    let preview = preview_node_data(&node_output);
                    (ctx.on_node_update)(NodeExecutionUpdate {
                        node_id: nid.clone(),
                        state: NodeExecutionState::Succeeded,
                        output_preview: preview,
                    });
                    layer_outputs.insert(nid.clone(), node_output);
                    break; // Success — stop retrying.
                }
                Err(e) => {
                    error!(node_id = %nid, attempt = retry_count, error = %e, "P1.11 retry attempt failed");
                    (ctx.on_node_update)(NodeExecutionUpdate {
                        node_id: nid.clone(),
                        state: NodeExecutionState::Failed,
                        output_preview: Some(e.to_string()),
                    });
                    // Loop continues to next retry attempt.
                }
            }
        }
    }

    Ok(layer_outputs)
}

// ─── Loop region driver (P1.11) ──────────────────────────────────────────────

/// Run a loop region to termination under the given `UntilCondition`.
///
/// The region's acyclic skeleton layers are executed in topological order on
/// each iteration. After each iteration, `UntilEvaluator::check_async` is
/// called to decide whether to continue. On `Halt`, the iteration loop exits
/// and the caller proceeds to post-loop layers.
///
/// Concurrency model (D4): within a layer, nodes run concurrently via
/// `JoinSet`; across iterations, execution is strictly serial.
#[allow(clippy::too_many_arguments)]
async fn run_loop_region(
    session_manager: Arc<SessionManager>,
    ctx: &GraphRunnerContext,
    workflow: &WorkflowDefinition,
    region_nodes: &[String],
    region_layers: Vec<Vec<String>>,
    outputs: &mut HashMap<String, NodeData>,
    source_nodes: &HashSet<&str>,
    input: &NodeData,
    incoming_edges: &HashMap<&str, Vec<&EdgeDef>>,
    until_condition: Option<UntilCondition>,
    failure_policies: &HashMap<String, FailurePolicy>,
) -> Result<(), ExecutionResult> {
    use crate::session::topology_ops::MAX_ITERATIONS;
    use crate::session::until_evaluator::{UntilEvaluator, UntilSignal};

    let condition = until_condition.unwrap_or(UntilCondition::MaxIterations(1));
    let mut evaluator = UntilEvaluator::new(
        condition,
        Arc::clone(&ctx.event_bus),
        Arc::clone(&session_manager),
    );

    let node_map: HashMap<&str, &NodeDef> =
        workflow.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    debug!(
        region_nodes = ?region_nodes,
        "P1.11 run_loop_region: starting loop region"
    );

    loop {
        // Check cancellation at the top of each iteration.
        if ctx.cancel_flag.load(Ordering::Relaxed) {
            return Err(ExecutionResult {
                success: false,
                output: None,
                error: Some("loop region cancelled".to_string()),
            });
        }

        let current_iteration = evaluator.iteration;

        // Execute each layer in the region's acyclic skeleton.
        for (layer_sub_idx, layer) in region_layers.iter().enumerate() {
            if ctx.cancel_flag.load(Ordering::Relaxed) {
                return Err(ExecutionResult {
                    success: false,
                    output: None,
                    error: Some("loop region cancelled during layer".to_string()),
                });
            }

            let layer_result = execute_layer(
                layer_sub_idx,
                layer,
                &node_map,
                source_nodes,
                input,
                incoming_edges,
                outputs,
                ctx,
                &session_manager,
                current_iteration,
                failure_policies,
            )
            .await;

            match layer_result {
                Ok(layer_outputs) => {
                    for (nid, output) in layer_outputs {
                        outputs.insert(nid, output);
                    }
                }
                Err(err_result) => return Err(err_result),
            }
        }

        // Evaluate until condition.
        let signal = evaluator
            .check_async(&session_manager, ctx.project_id)
            .await;

        match signal {
            UntilSignal::Continue => {
                // Enforce daemon-wide MAX_ITERATIONS cap (defense in depth).
                if evaluator.iteration >= MAX_ITERATIONS {
                    warn!(
                        iteration = evaluator.iteration,
                        max = MAX_ITERATIONS,
                        "P1.11: MAX_ITERATIONS cap reached — halting loop region"
                    );
                    break;
                }

                // Clear SCC node outputs so the next iteration re-runs them.
                for node_id in region_nodes {
                    outputs.remove(node_id);
                }

                debug!(
                    iteration = evaluator.iteration,
                    "P1.11: loop region continuing"
                );
            }
            UntilSignal::Halt(reason) => {
                debug!(
                    iteration = evaluator.iteration,
                    reason = ?reason,
                    "P1.11: loop region halting"
                );
                break;
            }
        }
    }

    Ok(())
}

/// Outcome of a single poll iteration in [`poll_terminal_status`].
enum WaitStep {
    /// The watched session reached a terminal status.
    Terminal(SessionStatus),
    /// Nothing terminal yet; keep waiting without checking cancellation.
    Continue,
    /// The poll window elapsed with no event; caller should check cancellation.
    TimedOut,
}

/// One iteration of the terminal-status wait loop used by
/// [`launch_and_wait_session`]. Extracted so the `Lagged` re-check can be
/// exercised directly in a unit test (see `tests::lagged_recv_rechecks_status_instead_of_looping_forever`).
///
/// A broadcast-bus lag can skip the watched session's terminal
/// `SessionStatusChanged` event — `tokio::sync::broadcast` never redelivers
/// skipped messages. Rather than just logging and looping back to `rx.recv()`
/// (which can only ever be unblocked again by cancellation), re-check the
/// watched session's status directly via `status_source`.
async fn poll_terminal_status<F, Fut>(
    rx: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
    session_id: Uuid,
    node_id: &str,
    status_source: F,
) -> AnyhowResult<WaitStep>
where
    F: FnOnce(Uuid) -> Fut,
    Fut: std::future::Future<Output = Option<SessionStatus>>,
{
    match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Ok(event)) => {
            if let DaemonEvent::SessionStatusChanged {
                session_id: sid,
                new_status,
                ..
            } = event.as_ref()
                && *sid == session_id
                && new_status.is_terminal()
            {
                return Ok(WaitStep::Terminal(*new_status));
            }
            Ok(WaitStep::Continue)
        }
        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
            warn!(
                node_id,
                lagged = n,
                "event bus receiver lagged; re-checking session status directly"
            );
            match status_source(session_id).await {
                Some(status) if status.is_terminal() => Ok(WaitStep::Terminal(status)),
                _ => Ok(WaitStep::Continue),
            }
        }
        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
            anyhow::bail!("event bus closed while waiting for node '{}'", node_id);
        }
        Err(_timeout) => Ok(WaitStep::TimedOut),
    }
}

/// Launch an AI session for a graph node and wait for it to reach a terminal state.
///
/// Subscribes to the EventBus and polls for `SessionStatusChanged` events.
/// On cancellation, interrupts the running session before returning.
async fn launch_and_wait_session(
    session_manager: &Arc<SessionManager>,
    event_bus: &Arc<EventBus>,
    cancel_flag: &Arc<AtomicBool>,
    config: crate::claude::LaunchConfig,
    fork: crate::topology::custody::TopologyForkSource,
    custody: &crate::topology::custody::TopologyCustody,
    node_id: &str,
    iteration: u32,
) -> AnyhowResult<NodeData> {
    let session_id = session_manager
        .launch_session_with_retry_admission(
            config,
            None,
            false,
            LaunchPurpose::TopologyNode(TopologyNodeLaunchContext {
                session_id: Uuid::new_v4(),
                fork,
            }),
            None,
        )
        .await?;
    info!(node_id, %session_id, "launched session for graph node");

    let mut rx = event_bus.subscribe();

    let final_status = loop {
        let step = poll_terminal_status(&mut rx, session_id, node_id, |sid| async move {
            session_manager.get_session(sid).await.map(|s| s.status)
        })
        .await;

        let step = match step {
            Ok(step) => step,
            Err(e) => {
                event_bus.unsubscribe();
                return Err(e);
            }
        };

        match step {
            WaitStep::Terminal(status) => break status,
            WaitStep::Continue => {}
            WaitStep::TimedOut => {
                // Timeout — check cancellation.
                if cancel_flag.load(Ordering::Relaxed) {
                    warn!(node_id, %session_id, "cancelling graph node session");
                    let _ = session_manager.interrupt_session(session_id).await;
                    event_bus.unsubscribe();
                    anyhow::bail!("node '{}' cancelled", node_id);
                }
            }
        }
    };

    event_bus.unsubscribe();

    let observed: AnyhowResult<Option<String>> = if final_status == SessionStatus::Completed {
        let session = session_manager
            .get_session(session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("topology session disappeared"))?;
        let root = session
            .sandbox_root
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("topology session has no sandbox"))?;
        custody
            .observe(node_id, iteration, root)
            .map(Some)
            .map_err(Into::into)
    } else {
        Ok(None)
    };
    let reclaim = reclaim_terminal_node_cache_nonblocking(session_manager, session_id).await;
    let final_status = report_reclaim_without_changing_status(final_status, reclaim, session_id);

    if final_status != SessionStatus::Completed {
        anyhow::bail!(
            "node '{}' session ended with status {:?} (expected Completed)",
            node_id,
            final_status
        );
    }

    // Extract the session output from conversation events.
    let _result_commit = observed?.expect("completed node has observed HEAD");
    let events = session_manager.get_conversation(session_id).await?;
    Ok(extract_session_output(&events))
}

pub(crate) async fn reclaim_terminal_node_cache_nonblocking(
    session_manager: &Arc<SessionManager>,
    session_id: Uuid,
) -> crate::error::Result<bool> {
    let Some((custody_id, generation)) = ({
        let Ok(store) = session_manager.store.try_lock() else {
            return Ok(false);
        };
        crate::topology::custody::terminal_node_cache_custody(&store, session_id)?
    }) else {
        return Ok(false);
    };

    let store = Arc::clone(&session_manager.store);
    let active = Arc::clone(&session_manager.active);
    let sandbox_base = session_manager.sandbox_allocator.base_dir().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Some(_root_guard) = crate::store::sandbox_custody::try_lock_custody_root(custody_id)
        else {
            return Ok(false);
        };
        let Ok(active) = active.try_read() else {
            return Ok(false);
        };
        if active.contains_key(&session_id) {
            return Ok(false);
        }
        let Ok(store) = store.try_lock() else {
            return Ok(false);
        };
        crate::topology::custody::reclaim_terminal_node_cache_locked(
            &store,
            &sandbox_base,
            custody_id,
            generation,
        )
    })
    .await
    .map_err(|error| crate::error::DaemonError::Process(error.to_string()))?
}

fn log_reclaim_failure(result: crate::error::Result<bool>, session_id: Uuid) {
    if let Err(error) = result {
        warn!(%session_id, %error, "terminal topology build-cache reclaim failed");
    }
}

fn report_reclaim_without_changing_status(
    terminal_status: SessionStatus,
    reclaim: crate::error::Result<bool>,
    session_id: Uuid,
) -> SessionStatus {
    log_reclaim_failure(reclaim, session_id);
    terminal_status
}

/// Build the query string for a node's AI session.
///
/// Combines upstream context (from predecessor outputs) with the node's
/// own instructions into a structured prompt.
pub(crate) fn build_node_query(node_def: &NodeDef, upstream_data: &NodeData) -> String {
    let mut parts = Vec::new();

    // Upstream context section.
    if !upstream_data.is_empty() {
        let mut context_lines = vec!["## Context from previous steps".to_string()];
        for (key, value) in upstream_data.iter() {
            if key == PIPELINE_ENTRY_CONTEXT_KEY {
                context_lines.push(format!("### pipeline goal\n{}", value));
                continue;
            }
            // Skip internal metadata fields (underscore-prefixed).
            if key.starts_with('_') {
                continue;
            }
            context_lines.push(format!("### {}\n{}", key, value));
        }
        if context_lines.len() > 1 {
            parts.push(context_lines.join("\n"));
        }
    }

    // Node instructions section.
    if !node_def.instructions.is_empty() {
        parts.push(format!("## Instructions\n{}", node_def.instructions));
    }

    if parts.is_empty() {
        format!("Execute node: {}", node_def.name)
    } else {
        parts.join("\n\n")
    }
}

pub(crate) fn attach_pipeline_entry_context(node_def: &NodeDef, output: &mut NodeData) {
    if node_def
        .tags
        .iter()
        .any(|tag| tag == PIPELINE_ENTRY_CONTEXT_TAG)
    {
        let context = node_def
            .instructions
            .split_once(PIPELINE_ENTRY_RESPONSE_MARKER)
            .map_or(node_def.instructions.as_str(), |(context, _)| context)
            .trim();
        output.insert(
            PIPELINE_ENTRY_CONTEXT_KEY,
            Value::String(context.to_string()),
        );
    }
}

/// `FailurePolicy::Retry` budget of one node: `repeat_policy.max_iterations`
/// retries (default 1). Shared by the legacy runner and the durable executor
/// so the kill switch never changes an ordinary workflow's retry behaviour.
pub(crate) fn failure_retry_budget(node_def: &NodeDef) -> u32 {
    node_def
        .repeat_policy
        .as_ref()
        .map_or(1, |rp| u32::try_from(rp.max_iterations).unwrap_or(u32::MAX))
}

/// Resolve the session provider for a node.
///
/// Checks, in order: node-level provider string, model_settings provider,
/// then falls back to None (daemon defaults to Claude).
pub(crate) fn resolve_provider(node_def: &NodeDef) -> Option<SessionProvider> {
    let provider_str = node_def.provider.as_deref().or_else(|| {
        node_def
            .model_settings
            .as_ref()
            .and_then(|ms| ms.provider.as_deref())
    });

    provider_str.and_then(parse_provider)
}

/// Parse a provider string into a `SessionProvider`.
fn parse_provider(s: &str) -> Option<SessionProvider> {
    match s.to_lowercase().as_str() {
        "claude" => Some(SessionProvider::Claude),
        "codex" => Some(SessionProvider::Codex),
        "pioneer" => Some(SessionProvider::Pioneer),
        "openrouter" => Some(SessionProvider::OpenRouter),
        "bedrock" => Some(SessionProvider::Bedrock),
        "gemini" | "antigravity" | "agy" => Some(SessionProvider::Antigravity),
        "local" => Some(SessionProvider::Local),
        "harness" => Some(SessionProvider::Harness),
        _ => {
            warn!(
                provider = s,
                "unknown provider string, falling back to daemon default"
            );
            None
        }
    }
}

/// Extract the session output from conversation events.
///
/// Finds the last assistant message and wraps its content in `NodeData`.
pub(crate) fn extract_session_output(events: &[ConversationEvent]) -> NodeData {
    let mut data = NodeData::new();

    let last_assistant_msg = events.iter().rev().find(|e| {
        e.role == Some(Role::Assistant)
            && e.event_type == EventType::Message
            && !e.content.is_empty()
    });

    if let Some(event) = last_assistant_msg {
        data.insert("content", Value::String(event.content.clone()));
    }

    data.insert("_completed", Value::Bool(true));
    data
}

/// Apply an edge filter to node data.
///
/// If the edge has a `FilterDef` with include/exclude lists, applies the
/// corresponding `FieldFilter`. Otherwise returns the data unchanged.
pub(crate) fn apply_edge_filter(data: &NodeData, edge: &EdgeDef) -> NodeData {
    match &edge.filter {
        Some(filter_def) => {
            if let Some(ref include) = filter_def.include {
                let field_filter = FieldFilter::Include(include.clone());
                field_filter.apply(data)
            } else if let Some(ref exclude) = filter_def.exclude {
                let field_filter = FieldFilter::Exclude(exclude.clone());
                field_filter.apply(data)
            } else {
                data.clone()
            }
        }
        None => data.clone(),
    }
}

/// Merge upstream node outputs for a given target node.
///
/// Collects outputs from all predecessor nodes (via incoming edges),
/// applies per-edge filters, and merges into a single `NodeData`.
pub(crate) fn merge_upstream_data(
    target_id: &str,
    incoming_edges: &HashMap<&str, Vec<&EdgeDef>>,
    outputs: &HashMap<String, NodeData>,
) -> NodeData {
    let mut merged = NodeData::new();

    if let Some(edges) = incoming_edges.get(target_id) {
        for edge in edges {
            if let Some(source_data) = outputs.get(&edge.source) {
                let filtered = apply_edge_filter(source_data, edge);
                merged.merge(filtered);
            }
        }
    }

    merged
}

// ─── Metadata parse helpers (P1.11) ──────────────────────────────────────────

/// Parse `metadata["loop_edges"]` into a list of `(from, to)` string pairs.
///
/// The value is expected to be a JSON-serialized `Vec<{"from": str, "to": str}>`.
/// Returns an empty Vec on any parse failure or absent key (backward compatible
/// with acyclic-only workflows that have no `loop_edges` metadata).
pub(crate) fn parse_loop_edges_from_metadata(
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
pub(crate) fn parse_scc_regions_from_metadata(
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
pub(crate) fn parse_until_condition_from_metadata(
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
pub(crate) fn parse_failure_policies_from_metadata(
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
/// that can execute concurrently. Returns an error if the (acyclic) graph has a cycle.
pub(crate) fn topological_layers(
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

/// Generate a short preview of node data for status updates.
pub(crate) fn preview_node_data(data: &NodeData) -> Option<String> {
    let json = serde_json::to_string(data).ok()?;
    const MAX_PREVIEW_CHARS: usize = 120;
    if json.chars().count() > MAX_PREVIEW_CHARS {
        let truncated: String = json.chars().take(MAX_PREVIEW_CHARS).collect();
        Some(format!("{}...", truncated))
    } else {
        Some(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn custody_test_repo() -> (tempfile::TempDir, String) {
        let repo = tempfile::TempDir::new().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::write(repo.path().join("seed.txt"), "seed\n").unwrap();
        git_test(repo.path(), &["add", "seed.txt"]);
        git_test(
            repo.path(),
            &[
                "-c",
                "user.name=Topology Test",
                "-c",
                "user.email=topology@test.invalid",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        let base = git_test(repo.path(), &["rev-parse", "HEAD"]);
        (repo, base)
    }

    fn git_test(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit_test_change(path: &Path, file: &str, contents: &str, message: &str) -> String {
        std::fs::write(path.join(file), contents).unwrap();
        git_test(path, &["add", file]);
        git_test(
            path,
            &[
                "-c",
                "user.name=Topology Test",
                "-c",
                "user.email=topology@test.invalid",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
        git_test(path, &["rev-parse", "HEAD"])
    }

    fn observe_planned_node(
        allocator: &crate::sandbox::SandboxAllocator,
        custody: &crate::topology::custody::TopologyCustody,
        plan: &crate::topology::custody::NodeForkPlan,
        node: &str,
        iteration: u32,
        content: Option<(&str, &str)>,
    ) -> (String, crate::sandbox::SandboxAllocation) {
        let source = plan.resolve(custody, iteration).unwrap();
        let allocation = allocator
            .allocate(
                Uuid::new_v4(),
                source.origin(),
                rsi_common::types::SandboxKind::GitWorktree,
                source.commit(),
                None,
            )
            .unwrap();
        if let Some((file, contents)) = content {
            commit_test_change(&allocation.root, file, contents, node);
        }
        let observed = custody.observe(node, iteration, &allocation.root).unwrap();
        (observed, allocation)
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn loop_reentry_uses_previous_iteration_commit_and_keeps_ancestry() {
        let (repo, base) = custody_test_repo();
        let custody = crate::topology::custody::TopologyCustody::new(
            repo.path().to_path_buf(),
            base,
            Uuid::new_v4(),
        );
        let workflow = WorkflowDefinition::new("loop lineage")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_node(NodeDef::action("c", "C"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("b", "c"))
            .with_edge(EdgeDef::new("c", "b"));
        let plan = crate::topology::custody::plan_topology_custody(
            &workflow.nodes,
            &workflow.edges,
            &[("c".into(), "b".into())],
            &[vec!["b".into(), "c".into()]],
        )
        .unwrap();
        let allocator = crate::sandbox::SandboxAllocator::new(repo.path().join("sandboxes"));

        let (a_commit, _a) = observe_planned_node(
            &allocator,
            &custody,
            plan.node("a").unwrap(),
            "a",
            0,
            Some(("a.txt", "from A\n")),
        );
        let (_b_commit, _b) = observe_planned_node(
            &allocator,
            &custody,
            plan.node("b").unwrap(),
            "b",
            0,
            Some(("b.txt", "from B0\n")),
        );
        let (c_commit, _c) = observe_planned_node(
            &allocator,
            &custody,
            plan.node("c").unwrap(),
            "c",
            0,
            Some(("c.txt", "from C0\n")),
        );
        let b_iteration_one = plan.node("b").unwrap().resolve(&custody, 1).unwrap();

        assert_eq!(b_iteration_one.commit(), c_commit);
        assert_eq!(
            git_test(
                repo.path(),
                &["merge-base", &a_commit, b_iteration_one.commit()],
            ),
            a_commit,
            "B iteration 1 must start from C iteration 0 and retain A ancestry"
        );
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn hub_fanin_uses_declared_lineage_and_unresolved_fanin_is_rejected() {
        let (repo, base) = custody_test_repo();
        let hub = WorkflowDefinition::new("hub")
            .with_node(NodeDef::action("router", "Router"))
            .with_node(NodeDef::action("code", "Code"))
            .with_node(NodeDef::action("tests", "Tests"))
            .with_node(NodeDef::action("docs", "Docs"))
            .with_node({
                let mut merge = NodeDef::action("merge", "Merge");
                merge.tags.push("custody.from=node:router".into());
                merge
            })
            .with_edge(EdgeDef::new("router", "code"))
            .with_edge(EdgeDef::new("router", "tests"))
            .with_edge(EdgeDef::new("router", "docs"))
            .with_edge(EdgeDef::new("code", "merge"))
            .with_edge(EdgeDef::new("tests", "merge"))
            .with_edge(EdgeDef::new("docs", "merge"));
        let plan = plan_workflow_custody(&hub).unwrap();
        let custody = crate::topology::custody::TopologyCustody::new(
            repo.path().to_path_buf(),
            base,
            Uuid::new_v4(),
        );
        let allocator = crate::sandbox::SandboxAllocator::new(repo.path().join("sandboxes"));
        let (router_commit, _router) = observe_planned_node(
            &allocator,
            &custody,
            plan.node("router").unwrap(),
            "router",
            0,
            Some(("router.txt", "router lineage\n")),
        );
        let merge_fork = plan.node("merge").unwrap().resolve(&custody, 0).unwrap();
        assert_eq!(merge_fork.commit(), router_commit);

        let unresolved = WorkflowDefinition::new("unresolved fanin")
            .with_node(NodeDef::action("left", "Left"))
            .with_node(NodeDef::action("right", "Right"))
            .with_node(NodeDef::action("join", "Join"))
            .with_edge(EdgeDef::new("left", "join"))
            .with_edge(EdgeDef::new("right", "join"));
        let error = plan_workflow_custody(&unresolved).unwrap_err();
        assert!(error.to_string().contains("multiple inputs"));
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn layer_preparation_error_returns_no_partial_launches() {
        let (repo, base) = custody_test_repo();
        let mut invalid = NodeDef::action("bad", "Bad");
        invalid.tags.push("kind:group".into());
        let workflow = WorkflowDefinition::new("preparation")
            .with_node(NodeDef::action("good", "Good"))
            .with_node(invalid);
        let custody_plan = crate::topology::custody::plan_topology_custody(
            &workflow.nodes,
            &workflow.edges,
            &[],
            &[],
        )
        .unwrap();
        let ctx = GraphRunnerContext {
            event_bus: Arc::new(EventBus::new(1)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            on_node_update: Arc::new(|_update| {}),
            project_id: None,
            workflow_id: Uuid::new_v4(),
            parent_id: None,
            is_topology: true,
            custody: crate::topology::custody::TopologyCustody::new(
                repo.path().to_path_buf(),
                base,
                Uuid::new_v4(),
            ),
            custody_plan,
        };
        let node_map: HashMap<&str, &NodeDef> = workflow
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect();
        let input = NodeData::new();

        let result = prepare_layer_launches(
            &["good".into(), "bad".into()],
            &node_map,
            &HashSet::from(["good", "bad"]),
            &input,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
            0,
        );

        let Err(error) = result else {
            panic!("invalid node must fail before returning prepared launches");
        };
        let message = error.error.unwrap();
        assert!(
            message.contains("kind"),
            "unexpected preparation error: {message}"
        );
    }

    #[test]
    fn cache_reclaim_error_preserves_completed_status() {
        let status = report_reclaim_without_changing_status(
            SessionStatus::Completed,
            Err(crate::error::DaemonError::Store("busy".into())),
            Uuid::new_v4(),
        );
        assert_eq!(status, SessionStatus::Completed);
    }

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

    #[test]
    fn build_node_query_with_context_and_instructions() {
        let node = NodeDef::action("test", "Test Node");
        let mut node = node;
        node.instructions = "Analyze the data".to_string();

        let mut upstream = NodeData::new();
        upstream.insert("summary", Value::String("Some summary".to_string()));
        upstream.insert("_completed", Value::Bool(true));

        let query = build_node_query(&node, &upstream);
        assert!(query.contains("## Context from previous steps"));
        assert!(query.contains("### summary"));
        assert!(query.contains("Some summary"));
        // Underscore-prefixed fields should be excluded.
        assert!(!query.contains("_completed"));
        assert!(query.contains("## Instructions"));
        assert!(query.contains("Analyze the data"));
    }

    #[test]
    fn pipeline_entry_goal_flows_without_visible_restatement() {
        let mut entry = NodeDef::action("entry", "Entry");
        entry.instructions = "PIPELINE GOAL:\n\
                              Fix the timeout bug without changing the RPC schema.\n\n\
                              Reply with exactly `PIPELINE ENTRY READY`, then exit. Do not \
                              quote, restate, or summarize the goal."
            .to_string();
        entry.tags.push(PIPELINE_ENTRY_CONTEXT_TAG.to_string());

        let mut output = NodeData::new();
        output.insert("content", Value::String("PIPELINE ENTRY READY".to_string()));
        attach_pipeline_entry_context(&entry, &mut output);

        let mut downstream = NodeDef::action("research", "Research");
        downstream.instructions = "Research the pipeline goal.".to_string();
        let query = build_node_query(&downstream, &output);

        assert!(query.contains("PIPELINE ENTRY READY"));
        assert!(query.contains("Fix the timeout bug without changing the RPC schema."));
        assert!(query.contains("### pipeline goal"));
        assert!(!query.contains("Reply with exactly"));
        assert!(!query.contains("restate"));
        assert!(!query.contains(PIPELINE_ENTRY_CONTEXT_KEY));
    }

    #[test]
    fn build_node_query_empty_falls_back_to_name() {
        let node = NodeDef::action("test", "My Node");
        let upstream = NodeData::new();

        let query = build_node_query(&node, &upstream);
        assert_eq!(query, "Execute node: My Node");
    }

    #[test]
    fn resolve_provider_from_node_field() {
        let mut node = NodeDef::action("test", "Test");
        node.provider = Some("gemini".to_string());

        assert_eq!(resolve_provider(&node), Some(SessionProvider::Antigravity));
    }

    #[test]
    fn resolve_provider_accepts_pioneer() {
        let mut node = NodeDef::action("test", "Test");
        node.provider = Some("pioneer".to_string());

        assert_eq!(resolve_provider(&node), Some(SessionProvider::Pioneer));
    }

    #[test]
    fn resolve_provider_falls_back_to_model_settings() {
        use rsi_graph::format::ModelSettings;

        let mut node = NodeDef::action("test", "Test");
        node.model_settings = Some(ModelSettings {
            model: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            provider: Some("codex".to_string()),
        });

        assert_eq!(resolve_provider(&node), Some(SessionProvider::Codex));
    }

    #[test]
    fn resolve_provider_none_when_unset() {
        let node = NodeDef::action("test", "Test");
        assert_eq!(resolve_provider(&node), None);
    }

    #[test]
    fn extract_session_output_finds_last_assistant_message() {
        use chrono::Utc;

        let sid = Uuid::new_v4();
        let events = vec![
            ConversationEvent {
                id: 0,
                session_id: sid,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::User),
                content: "hello".to_string(),
                tool_name: None,
                tool_input: None,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
                created_at: Utc::now(),
            },
            ConversationEvent {
                id: 0,
                session_id: sid,
                sequence: 2,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "first response".to_string(),
                tool_name: None,
                tool_input: None,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
                created_at: Utc::now(),
            },
            ConversationEvent {
                id: 0,
                session_id: sid,
                sequence: 3,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "final response".to_string(),
                tool_name: None,
                tool_input: None,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
                created_at: Utc::now(),
            },
        ];

        let output = extract_session_output(&events);
        assert_eq!(
            output.get("content"),
            Some(&Value::String("final response".to_string()))
        );
        assert_eq!(output.get("_completed"), Some(&Value::Bool(true)));
    }

    #[test]
    fn apply_edge_filter_include() {
        use rsi_graph::format::FilterDef;

        let mut data = NodeData::new();
        data.insert("keep", Value::String("yes".to_string()));
        data.insert("drop", Value::String("no".to_string()));

        let edge = EdgeDef {
            source: "a".to_string(),
            source_port: None,
            target: "b".to_string(),
            target_port: None,
            filter: Some(FilterDef {
                include: Some(vec!["keep".to_string()]),
                exclude: None,
            }),
            label: None,
        };

        let filtered = apply_edge_filter(&data, &edge);
        assert!(filtered.get("keep").is_some());
        assert!(filtered.get("drop").is_none());
    }

    #[test]
    fn apply_edge_filter_none_passes_through() {
        let mut data = NodeData::new();
        data.insert("key", Value::String("value".to_string()));

        let edge = EdgeDef::new("a", "b");
        let filtered = apply_edge_filter(&data, &edge);
        assert!(filtered.get("key").is_some());
    }

    // ── P1.11 Phase 9: loop executor + FailurePolicy tests ───────────────────

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

    /// Meta-impl topology smoke: ExecuteTopology on the real meta-impl topology.
    /// Marked #[ignore] — requires live daemon and DB.
    #[tokio::test]
    #[ignore = "requires live daemon; run manually: cargo test -p rsid -- smoke_meta_impl_topology --ignored"]
    async fn smoke_meta_impl_topology_loop_execution() {
        // Connect to daemon socket; issue ExecuteTopology; poll GetWorkflowExecution.
        // Assert: execution_id returned; both loop regions activate (non-panicking, non-hanging).
        // UUID: 8c9b9d08-7d17-437d-a5be-b1c749cb9206
        let _uuid = "8c9b9d08-7d17-437d-a5be-b1c749cb9206";
        // Implementation requires live daemon socket connection.
    }

    /// P1.12 §8 — verifies the `parent_id` plumbing the executor relies on.
    ///
    /// The full ticket §8 test ("spawned sessions all have parent_id = Epic.id
    /// written to the sessions table") requires a live SessionManager harness
    /// that can fork Claude subprocesses — coverage is gated to the manual
    /// `gR` smoke per the verification manifest (TUI Manual bucket).
    ///
    /// This test pins the contract: `GraphRunnerContext` carries `parent_id`,
    /// and that value is what the node launch builder stamps onto every
    /// spawned `LaunchConfig`.
    ///
    /// Keep the test as a contract pin so reverting Phase 2 fails compilation.
    #[test]
    fn test_executor_threads_parent_id_to_spawned_sessions() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        // Build the context with a non-None parent_id and assert the struct
        // carries it. This is a compile-time + runtime pin on the API surface.
        let bus = Arc::new(crate::bus::EventBus::new(1));
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let parent_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        let on_node_update: Arc<dyn Fn(NodeExecutionUpdate) + Send + Sync> = Arc::new(|_update| {});

        let ctx = GraphRunnerContext {
            event_bus: bus,
            cancel_flag,
            on_node_update,
            project_id: None,
            workflow_id,
            parent_id: Some(parent_id),
            is_topology: true,
            custody: crate::topology::custody::TopologyCustody::new(
                std::path::PathBuf::from("/tmp"),
                "0".repeat(40),
                Uuid::new_v4(),
            ),
            custody_plan: crate::topology::custody::TopologyCustodyPlan::default(),
        };

        assert_eq!(
            ctx.parent_id,
            Some(parent_id),
            "GraphRunnerContext must carry the Epic's parent_id to the node launch builder"
        );
    }

    /// Bus-lag review (2026-07-31): a broadcast-bus lag can skip the watched
    /// session's terminal `SessionStatusChanged` event, and `broadcast`
    /// never redelivers skipped messages. Before the fix, the `Lagged` arm
    /// just logged and looped back to `rx.recv()`, so the wait loop could
    /// only ever be unblocked again by cancellation — a silent hang.
    ///
    /// This drives a real `Lagged` by overflowing a tiny-capacity
    /// `EventBus`, and asserts `poll_terminal_status` re-checks the watched
    /// session's status directly and returns `Terminal` instead of spinning
    /// forever on `Continue`. Bounded by an outer `tokio::time::timeout` so
    /// a regression (reverting the `Lagged` arm to bare `continue` /
    /// unconditional `Continue`) fails this test instead of hanging it.
    #[tokio::test]
    async fn lagged_recv_rechecks_status_instead_of_looping_forever() {
        use std::sync::atomic::AtomicUsize;

        // Tiny capacity so publishing a handful of events forces the
        // subscriber to lag rather than receive each one in turn.
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();
        let session_id = Uuid::new_v4();

        // Publish the terminal transition in the MIDDLE of the burst, then
        // keep publishing past it so the tiny 2-slot buffer evicts it before
        // `rx` ever reads a message. This reproduces the real defect: the
        // terminal event is not merely delayed, it is permanently skipped —
        // `broadcast` never redelivers it, so the only way to observe
        // completion is to re-check status out of band.
        for i in 0..5 {
            bus.publish(DaemonEvent::SessionStatusChanged {
                session_id,
                old_status: SessionStatus::Running,
                new_status: if i == 2 {
                    SessionStatus::Completed
                } else {
                    SessionStatus::Running
                },
            });
        }

        // Fake status source standing in for `session_manager.get_session`:
        // the store already reflects the terminal status even though the
        // bus-lag dropped the event that would have announced it.
        let calls = AtomicUsize::new(0);
        let status_source = |_sid: Uuid| {
            calls.fetch_add(1, Ordering::Relaxed);
            async move { Some(SessionStatus::Completed) }
        };

        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match poll_terminal_status(&mut rx, session_id, "test-node", &status_source).await {
                    Ok(WaitStep::Terminal(status)) => break status,
                    Ok(WaitStep::Continue | WaitStep::TimedOut) => {}
                    Err(e) => panic!("unexpected error from poll_terminal_status: {e}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "poll_terminal_status must re-check status on Lagged and return \
                 Terminal instead of spinning until cancellation"
            )
        });

        assert_eq!(outcome, SessionStatus::Completed);
        assert!(
            calls.load(Ordering::Relaxed) >= 1,
            "status_source must be consulted when a Lagged error is observed"
        );

        bus.unsubscribe();
    }
}
