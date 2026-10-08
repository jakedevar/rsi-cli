//! Async workflow execution tracking for graph runs.

use super::SessionManager;
use crate::bus::DaemonEvent;
use crate::error::{DaemonError, Result};
use crate::graph_exec::{ExecutionHooks, NodeExecutionState, NodeExecutionUpdate};
use chrono::{DateTime, Duration, Utc};
use rsi_common::rpc::{ExecuteWorkflowResponse, InterruptWorkflowExecutionResponse};
use rsi_common::types::{
    GraphExecutionUpdate, MAX_SNAPSHOT_RULINGS, TopologyOnCallView, TopologyRulingView,
    WorkflowExecutionLookup, WorkflowExecutionSnapshot, WorkflowExecutionStatus,
    WorkflowNodeExecutionState, WorkflowValidationReport,
};
use rsi_graph::data::NodeData;
use rsi_graph::format::WorkflowDefinition;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

const COMPLETED_EXECUTION_RETENTION_LIMIT: usize = 256;
const EXPIRED_EXECUTION_RETENTION_LIMIT: usize = 256;
const EXECUTION_RETENTION_TTL_HOURS: i64 = 24;

pub(crate) struct TrackedWorkflowExecution {
    pub snapshot: WorkflowExecutionSnapshot,
    pub cancel_flag: Arc<AtomicBool>,
}

#[derive(Default)]
pub(crate) struct WorkflowExecutionRegistry {
    pub entries: HashMap<Uuid, TrackedWorkflowExecution>,
    pub expired: HashMap<Uuid, DateTime<Utc>>,
}

impl SessionManager {
    /// Start a workflow execution and return immediately with the execution ID.
    pub async fn execute_workflow(
        &self,
        workflow_id: Uuid,
        workflow: WorkflowDefinition,
        input: Option<serde_json::Value>,
        dry_run: bool,
    ) -> Result<ExecuteWorkflowResponse> {
        let validation = rsi_graph::validate_executable_workflow(&workflow);
        if validation.has_errors() {
            return Err(DaemonError::InvalidParam(validation_summary(&validation)));
        }
        // #635: typed steps, edge routing and refused author inputs are
        // re-validated at execute, before any row or launch.
        let steps = crate::topology::steps::validate_workflow(&workflow)
            .map_err(DaemonError::InvalidParam)?;
        if !dry_run && steps.has_typed_effects() {
            return Err(DaemonError::InvalidParam(
                "command and gate nodes run only on the durable topology executor".into(),
            ));
        }

        let execution_id = Uuid::new_v4();
        let accepted_at = Utc::now();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let snapshot = WorkflowExecutionSnapshot {
            execution_id,
            workflow_id,
            workflow_name: workflow.name.clone(),
            status: WorkflowExecutionStatus::Accepted,
            accepted_at,
            started_at: None,
            finished_at: None,
            dry_run,
            input: input.clone(),
            output: None,
            error: None,
            last_sequence: 0,
            row_version: None,
            blocked_attempt_id: None,
            blocked_reason: None,
            waiting: None,
            current_nodes: Vec::new(),
            on_call: None,
            rulings: Vec::new(),
            updates: Vec::new(),
        };

        {
            let mut executions = lock_workflow_executions(self.workflow_executions())?;
            executions.entries.insert(
                execution_id,
                TrackedWorkflowExecution {
                    snapshot,
                    cancel_flag: Arc::clone(&cancel_flag),
                },
            );
        }

        let accepted_update = append_execution_update(
            self.workflow_executions(),
            execution_id,
            UpdateSpec {
                node_id: None,
                status: WorkflowExecutionStatus::Accepted,
                node_state: None,
                finished: false,
                error: None,
                output_preview: None,
                mark_started: false,
                final_output: None,
            },
        )?;
        self.event_bus().publish(DaemonEvent::GraphExecution {
            update: accepted_update,
        });

        let executions = Arc::clone(self.workflow_executions());
        let event_bus = Arc::clone(self.event_bus());

        tokio::task::spawn_blocking(move || {
            if let Err(error) = run_execution_task(
                executions,
                event_bus,
                execution_id,
                workflow,
                input,
                dry_run,
                cancel_flag,
            ) {
                tracing::error!(
                    error = %error,
                    execution_id = %execution_id,
                    "Workflow execution task failed"
                );
            }
        });

        Ok(ExecuteWorkflowResponse {
            execution_id,
            workflow_id,
            accepted_at,
            dry_run,
        })
    }

    /// Start a live (non-dry-run) workflow execution using the async graph runner.
    ///
    /// Unlike the dry-run path which uses the synchronous `DagExecutor`,
    /// this spawns a tokio task that launches real AI sessions for each node.
    ///
    /// This is an associated function (not `&self`) so the `Arc<SessionManager>`
    /// can be moved into the spawned task.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_workflow_live(
        session_manager: Arc<SessionManager>,
        workflow_id: Uuid,
        workflow: WorkflowDefinition,
        input: Option<serde_json::Value>,
        dry_run: bool,
        project_id: Option<Uuid>,
        working_dir: Option<std::path::PathBuf>,
        parent_id: Option<Uuid>,
    ) -> Result<ExecuteWorkflowResponse> {
        let steps = validate_live_workflow(&workflow)?;
        if !dry_run && steps.has_typed_effects() && !session_manager.topology_executor_enabled() {
            return Err(DaemonError::InvalidParam(
                "command and gate nodes need the durable topology executor (topology_executor_enabled)"
                    .into(),
            ));
        }

        // #1746: review and land nodes need an owning project manager. The
        // daemon resolves it from the Epic's project, never from the request;
        // an operator start without one is refused with a typed reason.
        let owner = if !dry_run && (steps.has_review_nodes() || steps.has_land_nodes()) {
            let store = session_manager.store.lock().await;
            Some(crate::topology::agent::resolve_operator_owner(
                &store, project_id, parent_id,
            )?)
        } else {
            None
        };
        let custody_base = if dry_run {
            None
        } else {
            let repo = working_dir.clone().unwrap_or(std::env::current_dir()?);
            let explicit_base = input
                .as_ref()
                .and_then(|value| value.get("base_commit"))
                .and_then(serde_json::Value::as_str);
            Some(
                resolve_live_custody_base(
                    &session_manager.store,
                    &workflow,
                    &steps,
                    &repo,
                    explicit_base,
                )
                .await?,
            )
        };
        let custody_plan = if dry_run {
            None
        } else {
            Some(super::graph_runner::plan_workflow_custody(&workflow)?)
        };

        // #1641 S5a: an integer `inputs.issue` becomes the Issue snapshot at
        // accept, so node sessions need no Issue binding.
        let input = if dry_run {
            input
        } else {
            let store = session_manager.store.lock().await;
            crate::topology::starters::resolve_issue_input(&store, project_id, input)?
        };

        // #634: live executions are durable unless the operator kill switch
        // routes them to the legacy in-memory runner (rollback path).
        if let (Some(plan), Some((repo_root, base_commit))) = (&custody_plan, &custody_base)
            && session_manager.topology_executor_enabled()
        {
            return Self::accept_durable_execution(
                &session_manager,
                crate::topology::store::NewExecution {
                    id: Uuid::new_v4(),
                    topology_id: source_topology_id(&workflow),
                    workflow_id,
                    definition: workflow,
                    custody_plan: plan.clone(),
                    project_id,
                    parent_session_id: parent_id,
                    repo_root: repo_root.clone(),
                    base_commit: base_commit.clone(),
                    input,
                    requester: None,
                    owner,
                },
            )
            .await;
        }

        let execution_id = Uuid::new_v4();
        let accepted_at = Utc::now();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let snapshot = WorkflowExecutionSnapshot {
            execution_id,
            workflow_id,
            workflow_name: workflow.name.clone(),
            status: WorkflowExecutionStatus::Accepted,
            accepted_at,
            started_at: None,
            finished_at: None,
            dry_run,
            input: input.clone(),
            output: None,
            error: None,
            last_sequence: 0,
            row_version: None,
            blocked_attempt_id: None,
            blocked_reason: None,
            waiting: None,
            current_nodes: Vec::new(),
            on_call: None,
            rulings: Vec::new(),
            updates: Vec::new(),
        };

        {
            let mut executions = lock_workflow_executions(session_manager.workflow_executions())?;
            executions.entries.insert(
                execution_id,
                TrackedWorkflowExecution {
                    snapshot,
                    cancel_flag: Arc::clone(&cancel_flag),
                },
            );
        }

        let accepted_update = append_execution_update(
            session_manager.workflow_executions(),
            execution_id,
            UpdateSpec {
                node_id: None,
                status: WorkflowExecutionStatus::Accepted,
                node_state: None,
                finished: false,
                error: None,
                output_preview: None,
                mark_started: false,
                final_output: None,
            },
        )?;
        session_manager
            .event_bus()
            .publish(DaemonEvent::GraphExecution {
                update: accepted_update,
            });

        if dry_run {
            // Dry-run: use existing synchronous DagExecutor path.
            let executions = Arc::clone(session_manager.workflow_executions());
            let event_bus = Arc::clone(session_manager.event_bus());

            tokio::task::spawn_blocking(move || {
                if let Err(error) = run_execution_task(
                    executions,
                    event_bus,
                    execution_id,
                    workflow,
                    input,
                    dry_run,
                    cancel_flag,
                ) {
                    tracing::error!(
                        error = %error,
                        execution_id = %execution_id,
                        "Workflow execution task failed"
                    );
                }
            });
        } else {
            // Live execution: use the async graph runner.
            let executions = Arc::clone(session_manager.workflow_executions());
            let event_bus = Arc::clone(session_manager.event_bus());
            let custody_plan =
                custody_plan.expect("live execution plans custody before acceptance");

            tokio::spawn(run_async_execution_task(
                session_manager,
                executions,
                event_bus,
                execution_id,
                workflow_id,
                workflow,
                input,
                cancel_flag,
                project_id,
                parent_id,
                custody_plan,
                custody_base.map(|(root, commit)| {
                    crate::topology::custody::TopologyCustody::new(root, commit, execution_id)
                }),
            ));
        }

        Ok(ExecuteWorkflowResponse {
            execution_id,
            workflow_id,
            accepted_at,
            dry_run,
        })
    }

    /// Fetch the current or retained lookup result for a workflow execution.
    /// In-memory (dry-run and legacy) executions keep their retention
    /// semantics; durable executions project from `topology_events` and are
    /// never expired.
    pub async fn get_workflow_execution(
        &self,
        execution_id: Uuid,
    ) -> Result<WorkflowExecutionLookup> {
        let lookup = {
            let mut executions = lock_workflow_executions(self.workflow_executions())?;
            prune_execution_registry(&mut executions, Utc::now());
            lookup_execution(&executions, execution_id)
        };
        if !matches!(lookup, WorkflowExecutionLookup::NotFound { .. }) {
            return Ok(lookup);
        }
        let store = self.store.lock().await;
        let Some(mut execution) = crate::topology::store::execution_snapshot(&store, execution_id)?
        else {
            return Ok(lookup);
        };
        attach_run_view(&store, &mut execution)?;
        Ok(WorkflowExecutionLookup::Found { execution })
    }

    /// Request interruption for a running workflow execution.
    pub async fn interrupt_workflow_execution(
        self: &Arc<Self>,
        execution_id: Uuid,
    ) -> Result<Option<InterruptWorkflowExecutionResponse>> {
        let interrupt_requested_at = Utc::now();
        {
            let mut executions = lock_workflow_executions(self.workflow_executions())?;
            prune_execution_registry(&mut executions, interrupt_requested_at);

            if let Some(entry) = executions.entries.get_mut(&execution_id) {
                entry.cancel_flag.store(true, Ordering::Relaxed);

                return Ok(Some(InterruptWorkflowExecutionResponse {
                    execution_id,
                    status: entry.snapshot.status,
                    interrupt_requested_at,
                }));
            }
        }
        let executor = self.topology_executor().await;
        let Some(status) = executor.request_interrupt(execution_id).await? else {
            return Ok(None);
        };
        self.drive_topology_execution(executor, execution_id);
        Ok(Some(InterruptWorkflowExecutionResponse {
            execution_id,
            status: status.wire(),
            interrupt_requested_at,
        }))
    }

    /// Operator-only resolution of a preserved-work attempt (plan §3.4).
    pub(crate) async fn resolve_topology_attempt(
        self: &Arc<Self>,
        params: &rsi_common::rpc::ResolveTopologyAttemptParams,
    ) -> Result<rsi_common::rpc::ResolveTopologyAttemptResponse> {
        let executor = self.topology_executor().await;
        let response = executor.resolve_attempt(params).await?;
        if !response.deduplicated {
            self.drive_topology_execution(executor, params.execution_id);
        }
        Ok(response)
    }

    pub(crate) fn topology_executor_enabled(&self) -> bool {
        self.runtime_config
            .topology_executor_enabled
            .load(Ordering::Relaxed)
    }

    pub(crate) async fn topology_executor(
        self: &Arc<Self>,
    ) -> crate::topology::executor::Executor<SessionNodeEffects> {
        let boot_id = self.store.lock().await.program_run_boot_id();
        crate::topology::executor::Executor::new(
            Arc::clone(&self.store),
            Arc::new(SessionNodeEffects {
                manager: Arc::clone(self),
            }),
            boot_id,
            Arc::clone(&self.topology_drivers),
        )
    }

    pub(crate) fn drive_topology_execution(
        self: &Arc<Self>,
        executor: crate::topology::executor::Executor<SessionNodeEffects>,
        execution_id: Uuid,
    ) {
        if !self.topology_executor_enabled() {
            return;
        }
        crate::topology::executor::spawn_driver(
            executor,
            Arc::clone(self.event_bus()),
            execution_id,
        );
    }

    async fn accept_durable_execution(
        session_manager: &Arc<Self>,
        new: crate::topology::store::NewExecution,
    ) -> Result<ExecuteWorkflowResponse> {
        let execution_id = new.id;
        let workflow_id = new.workflow_id;
        let accepted = {
            let store = session_manager.store.lock().await;
            crate::topology::store::insert_execution(&store, &new)?
        };
        let accepted_at = accepted.updated_at;
        session_manager
            .event_bus()
            .publish(DaemonEvent::GraphExecution { update: accepted });
        let executor = session_manager.topology_executor().await;
        session_manager.drive_topology_execution(executor, execution_id);
        Ok(ExecuteWorkflowResponse {
            execution_id,
            workflow_id,
            accepted_at,
            dry_run: false,
        })
    }

    /// Startup: one bounded recovery pass, then a supervisor that adopts any
    /// drivable execution without a live driver (plan §2.4) and sweeps
    /// expired failure pins hourly.
    pub async fn start_topology_executor(
        self: Arc<Self>,
        max_executions: usize,
        time_budget: std::time::Duration,
    ) {
        // #633: native agent topology tools reach the executor from here on.
        let _ = self.topology_agent_self.set(Arc::downgrade(&self));
        // #1641 S5a: the starter topologies exist before any agent lists them.
        crate::topology::starters::seed_all(&*self.store.lock().await);
        let executor = self.topology_executor().await;
        match crate::topology::recovery::recover_after_restart(
            &executor,
            max_executions,
            time_budget,
        )
        .await
        {
            Ok(report) => {
                if !report.advanced.is_empty()
                    || report.deferred
                    || report.cleanup_steps > 0
                    || report.discards_completed > 0
                {
                    tracing::info!(
                        advanced = report.advanced.len(),
                        deferred = report.deferred,
                        cleanup_steps = report.cleanup_steps,
                        discards_completed = report.discards_completed,
                        "Topology executor restart recovery pass complete"
                    );
                }
                for (execution_id, step) in report.advanced {
                    if step == crate::topology::executor::Step::Wait {
                        self.drive_topology_execution(executor.clone(), execution_id);
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "Topology executor restart recovery deferred"),
        }
        tokio::spawn(async move {
            let mut ticks: u64 = 0;
            loop {
                tokio::time::sleep(crate::topology::executor::EXECUTOR_TICK).await;
                ticks = ticks.wrapping_add(1);
                if !self.topology_executor_enabled() {
                    continue;
                }
                let ids = {
                    let store = self.store.lock().await;
                    crate::topology::store::drivable_execution_ids(&store, 256)
                };
                match ids {
                    Ok(ids) => {
                        for execution_id in ids {
                            if !self.topology_drivers.is_driving(execution_id) {
                                self.drive_topology_execution(executor.clone(), execution_id);
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "topology supervisor scan failed"),
                }
                if ticks.is_multiple_of(120) {
                    crate::topology::recovery::settlement_cleanup(&executor, 256).await;
                    if let Err(error) = executor.complete_pending_discards(256).await {
                        tracing::warn!(%error, "pending topology discards deferred");
                    }
                }
            }
        });
    }
}

/// Structural validation shared by every live execution path: the graph,
/// typed steps, edge routing and refused author inputs (#635).
pub(crate) fn validate_live_workflow(
    workflow: &WorkflowDefinition,
) -> Result<crate::topology::steps::WorkflowSteps> {
    // A declared loop edge closes a cycle by design (the topology validator
    // bounds it with `until`/`max_iterations`); the graph's acyclicity check
    // covers the forward edges (#1641 S5a: the review/fix loop). Only a region
    // with a single loop edge is relaxed; more stay refused as cycles.
    let loops = super::graph_runner::parse_loop_edges_from_metadata(&workflow.metadata);
    let regions = super::graph_runner::parse_scc_regions_from_metadata(&workflow.metadata);
    let one_loop_per_region = regions.iter().all(|region| {
        loops
            .iter()
            .filter(|(from, to)| region.contains(from) && region.contains(to))
            .count()
            <= 1
    });
    let validation = if loops.is_empty() || !one_loop_per_region {
        rsi_graph::validate_executable_workflow(workflow)
    } else {
        let mut forward = workflow.clone();
        forward
            .edges
            .retain(|edge| !loops.contains(&(edge.source.clone(), edge.target.clone())));
        rsi_graph::validate_executable_workflow(&forward)
    };
    if validation.has_errors() {
        return Err(DaemonError::InvalidParam(validation_summary(&validation)));
    }
    crate::topology::steps::validate_workflow(workflow).map_err(DaemonError::InvalidParam)
}

/// The execution repository and its authenticated base commit (plan §4),
/// with the node working-dir and catalog-crate checks (plan §3.2). Shared by
/// the operator `ExecuteTopology` path and the agent verbs (#633).
pub(crate) async fn resolve_live_custody_base(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    workflow: &WorkflowDefinition,
    steps: &crate::topology::steps::WorkflowSteps,
    repo: &std::path::Path,
    explicit_base: Option<&str>,
) -> Result<(std::path::PathBuf, String)> {
    if store.lock().await.path_is_inside_live_custody_root(repo)? {
        return Err(DaemonError::InvalidParam(
            "execution repository is an active sandbox".into(),
        ));
    }
    let (root, commit) =
        crate::topology::custody::resolve_execution_base(repo, explicit_base).await?;
    for node in &workflow.nodes {
        if let Some(node_dir) = node.working_dir.as_ref() {
            let node_root = crate::topology::custody::repository_root(node_dir)?;
            if node_root != root || node_dir.canonicalize()? != root {
                return Err(DaemonError::InvalidParam(format!(
                    "node {} working_dir must name the execution repository",
                    node.id
                )));
            }
        }
    }
    // Plan §3.2: a catalog crate must be a workspace member.
    let crates = steps.command_crates();
    if !crates.is_empty() {
        let members = crate::topology::catalog::workspace_members(&root).await?;
        if let Some((node, krate)) = crates.iter().find(|(_, krate)| !members.contains(*krate)) {
            return Err(DaemonError::InvalidParam(format!(
                "node {node}: crate {krate} is not a workspace member"
            )));
        }
    }
    Ok((root, commit))
}

fn source_topology_id(workflow: &WorkflowDefinition) -> Option<Uuid> {
    match workflow.metadata.get("source_topology_id") {
        Some(rsi_graph::data::Value::String(id)) => Uuid::parse_str(id).ok(),
        _ => None,
    }
}

/// Production effects of the durable topology executor: node sessions are
/// ordinary agent sessions launched through the fenced launch path.
pub(crate) struct SessionNodeEffects {
    manager: Arc<SessionManager>,
}

/// A keyed one-shot test barrier at the former classification/cursor gap.
/// It holds no runtime or store locks: the competing turn runs to completion.
#[cfg(test)]
fn restart_observation_pauses() -> &'static Mutex<
    HashMap<
        Uuid,
        (
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        ),
    >,
> {
    type Pauses = Mutex<
        HashMap<
            Uuid,
            (
                tokio::sync::oneshot::Sender<()>,
                tokio::sync::oneshot::Receiver<()>,
            ),
        >,
    >;
    static PAUSES: std::sync::OnceLock<Pauses> = std::sync::OnceLock::new();
    PAUSES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn install_restart_observation_pause(
    id: Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    restart_observation_pauses()
        .lock()
        .unwrap()
        .insert(id, (reached_tx, resume_rx));
    (reached_rx, resume_tx)
}

#[cfg(test)]
async fn pause_restart_observation_for_test(id: Uuid) {
    let pause = restart_observation_pauses().lock().unwrap().remove(&id);
    if let Some((reached, resume)) = pause {
        let _ = reached.send(());
        let _ = resume.await;
    }
}

impl crate::topology::executor::NodeEffects for SessionNodeEffects {
    fn enabled(&self) -> bool {
        self.manager.topology_executor_enabled()
    }

    async fn launch(&self, request: crate::topology::executor::LaunchRequest) -> Result<Uuid> {
        self.manager
            .boxed_launch_session_with_retry_admission(
                request.config,
                None,
                false,
                super::types::LaunchPurpose::TopologyNode(
                    super::types::TopologyNodeLaunchContext {
                        session_id: request.session_id,
                        fork: request.fork,
                    },
                ),
                None,
            )
            .await
    }

    async fn session(
        &self,
        session_id: Uuid,
    ) -> Option<crate::topology::executor::SessionObservation> {
        // The terminal classification must not come from the completed cache
        // and then acquire a cursor from a later turn. Capture all durable
        // facts in one store-lock hold and read transaction (#1728 round 3).
        let observation = self
            .manager
            .store
            .lock()
            .await
            .restart_cut_observation(session_id);
        let (durable, restart_cut_fence) = match observation {
            Ok(Some(observation)) => observation,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%session_id, %error, "topology session observation failed");
                // A read failure supplies no usable recovery fence. Retain the
                // ordinary session view; resume_cut_session defers, fail closed.
                let session = self.manager.get_session(session_id).await?;
                return Some(crate::topology::executor::SessionObservation {
                    resumable: session.provider
                        != rsi_common::types::SessionProvider::CodexAppServer
                        && super::lifecycle::resumable_provider_session_id(&session).is_some(),
                    status: session.status,
                    sandbox_root: session.sandbox_root,
                    waiting_since: session.approval_started_at,
                    waited_ms: session.approval_wait_ms,
                    stop_reason: session.stop_reason,
                    restart_intent_pending: false,
                    restart_cut_fence: None,
                });
            }
        };
        let ended = matches!(
            durable.status,
            rsi_common::types::SessionStatus::Interrupted
                | rsi_common::types::SessionStatus::Failed
        );
        // Live waits retain their in-memory, monotonic-clock projection.
        let session = if durable.status.is_terminal() {
            durable
        } else {
            self.manager
                .get_session(session_id)
                .await
                .unwrap_or(durable)
        };
        // This is the old classification -> cursor gap. The snapshot already
        // owns BOTH, so a real continuation here cannot refresh only the cursor.
        #[cfg(test)]
        if ended {
            pause_restart_observation_for_test(session_id).await;
        }
        let restart_intent_pending = ended && restart_cut_fence.restart_intent_pending;
        Some(crate::topology::executor::SessionObservation {
            resumable: session.provider != rsi_common::types::SessionProvider::CodexAppServer
                && super::lifecycle::resumable_provider_session_id(&session).is_some(),
            restart_intent_pending,
            stop_reason: session.stop_reason.clone(),
            status: session.status,
            sandbox_root: session.sandbox_root,
            waiting_since: session.approval_started_at,
            waited_ms: session.approval_wait_ms,
            restart_cut_fence: ended.then_some(restart_cut_fence),
        })
    }

    async fn output(&self, session_id: Uuid) -> Result<NodeData> {
        let events = self.manager.get_conversation(session_id).await?;
        Ok(super::graph_runner::extract_session_output(&events))
    }

    async fn interrupt(&self, session_id: Uuid) {
        if let Err(error) = self
            .manager
            .interrupt_session_from(
                session_id,
                crate::terminal_cause::InterruptSource::GraphCancel,
            )
            .await
        {
            tracing::debug!(%session_id, %error, "topology node interrupt not delivered");
        }
    }

    async fn reclaim(&self, session_id: Uuid) {
        let reclaim =
            super::graph_runner::reclaim_terminal_node_cache_nonblocking(&self.manager, session_id)
                .await;
        if let Err(error) = reclaim {
            tracing::warn!(%session_id, %error, "terminal topology build-cache reclaim failed");
        }
    }

    /// Idempotent: a missing or already archived session is released.
    async fn release_sandbox(&self, session_id: Uuid) -> Result<()> {
        match self.manager.get_session(session_id).await {
            None => Ok(()),
            Some(session)
                if matches!(
                    session.status,
                    rsi_common::types::SessionStatus::Archived
                        | rsi_common::types::SessionStatus::Deleted
                ) =>
            {
                Ok(())
            }
            Some(_) => self.manager.archive_session(session_id).await.map(|_| ()),
        }
    }

    async fn predicate_met(&self, predicate: &str, project_id: Option<Uuid>) -> bool {
        predicate == "index_exhausted"
            && super::until_evaluator::check_predicate_index_exhausted(&self.manager, project_id)
                .await
    }

    fn publish(&self, update: GraphExecutionUpdate) {
        self.manager
            .event_bus()
            .publish(DaemonEvent::GraphExecution { update });
    }

    fn notify_operator(&self, level: &str, message: String) {
        tracing::warn!("{message}");
        self.manager
            .event_bus()
            .publish(DaemonEvent::SystemMessage {
                level: level.into(),
                message,
            });
    }

    /// #1715: the on-call ruling's answer continues the node's own session
    /// through the normal continue path, bound to the exact execution, attempt
    /// and decision and durably claimed before the provider effect.
    async fn continue_with_answer(
        &self,
        request: crate::topology::executor::AnswerContinuation,
    ) -> Result<()> {
        self.manager
            .continue_topology_answer(request.session_id, request.prompt, request.binding)
            .await
    }

    /// #1641 S3c: the on-call ruling continues the node's own session through
    /// the normal continue path (the same one a resumed worker uses).
    async fn continue_session(&self, session_id: Uuid, prompt: String) -> Result<()> {
        self.manager.continue_session(session_id, prompt).await
    }

    /// #1728: the restart resume takes the K2 fenced continuation like a
    /// stall nudge, so a competing continuation of the same session refuses
    /// one of the two (`continuation_target_busy`) instead of racing it.
    async fn resume_cut_session(
        &self,
        session_id: Uuid,
        prompt: String,
        observed_restart_cut: Option<crate::store::manager_actions::fence::RestartCutFenceV1>,
    ) -> Result<()> {
        let observed_restart_cut = observed_restart_cut.ok_or_else(|| {
            DaemonError::InvalidParam(
                crate::store::manager_actions::fence::CONTINUATION_TURN_CHANGED.into(),
            )
        })?;
        // The cursor comes from the observation that classified the session
        // as restart-cut, not from this capture: the guarded check refuses
        // `continuation_turn_changed` if a competing continuation ran since.
        let mut fence = self
            .manager
            .capture_exact_continuation_fence(
                session_id,
                crate::store::manager_actions::fence::ContinuationAuthorityV1::Automated,
            )
            .await?;
        fence.observed_restart_cut = Some(observed_restart_cut);
        self.manager
            .continue_fenced(session_id, prompt, fence)
            .await
    }

    async fn allocate_command_sandbox(
        &self,
        session_id: Uuid,
        fork: crate::topology::custody::TopologyForkSource,
    ) -> Result<std::path::PathBuf> {
        let allocator = Arc::clone(&self.manager.sandbox_allocator);
        let permit = self.manager.admit_sandbox_allocation().await?;
        tokio::task::spawn_blocking(move || {
            crate::topology::catalog::allocate_or_adopt_with_permit(
                &allocator, permit, session_id, &fork,
            )
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    async fn start_command(
        &self,
        attempt_id: Uuid,
        sandbox: std::path::PathBuf,
        op: rsi_common::types::CatalogOp,
    ) -> Result<Option<i32>> {
        self.manager
            .topology_drivers
            .commands
            .start(attempt_id, sandbox, op)
            .await
    }

    fn poll_command(&self, attempt_id: Uuid) -> Option<crate::topology::catalog::CommandPoll> {
        self.manager.topology_drivers.commands.poll(attempt_id)
    }

    fn cancel_command(&self, attempt_id: Uuid) {
        self.manager.topology_drivers.commands.cancel(attempt_id);
    }

    fn forget_command(&self, attempt_id: Uuid) {
        self.manager.topology_drivers.commands.forget(attempt_id);
    }

    fn kill_stale_group(&self, pgid: i32, sandbox: Option<&std::path::Path>) {
        crate::topology::catalog::kill_group(pgid, sandbox);
    }

    fn build_node_cap(&self) -> u32 {
        self.manager
            .runtime_config
            .topology_max_concurrent_build_nodes
            .load(Ordering::Relaxed)
    }

    fn build_slot_held(
        &self,
        attempt: &crate::topology::store::AttemptRow,
    ) -> Option<&'static str> {
        self.manager.topology_drivers.commands.build_slot(
            crate::governor::Governor::global(),
            &self.manager.runtime_config.governor_policy(),
            attempt.id,
        )
    }

    fn release_build_slot(&self, attempt_id: Uuid) {
        self.manager.topology_drivers.commands.release_build_slot(
            crate::governor::Governor::global(),
            &self.manager.runtime_config.governor_policy(),
            attempt_id,
        );
    }

    fn launch_held(
        &self,
        unattended: bool,
        since: chrono::DateTime<chrono::Utc>,
        attempt: &crate::topology::store::AttemptRow,
    ) -> bool {
        // #1641 S4b: the first launches of an operator-requested execution
        // start when asked; every other node launch waits for the host.
        unattended
            && self
                .manager
                .host_load
                .admit_waiter(attempt.id, since, Some(attempt.session_id))
                .is_some()
    }

    /// #1641 S1b: a review node's request on the owning project manager's
    /// ledger. The executor is the actor; the reviewer launches on the
    /// ordinary DB-native review path, so its family policy, round budget and
    /// receipts are the manager's own.
    async fn request_review(
        &self,
        request: crate::topology::review::ReviewRequest,
    ) -> Result<Uuid> {
        use crate::store::manager_reviews::{REVIEW_NO_MANAGER_LEDGER, TopologyReviewRequest};
        let (Some(project_id), Some(epic_id)) = (request.project_id, request.epic_id) else {
            return Err(DaemonError::PolicyDenied(format!(
                "{REVIEW_NO_MANAGER_LEDGER}: the execution has no project and Epic to own a review"
            )));
        };
        // The reviewer forks this commit from the repository: prove the
        // executor pinned exactly it before a ledger row names it.
        let pinned = {
            let (repo, pin, commit) = (
                request.repo_root.clone(),
                request.pin_ref.clone(),
                request.source_commit.clone(),
            );
            tokio::task::spawn_blocking(move || {
                crate::topology::custody::ref_points_at(&repo, &pin, &commit)
            })
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))?
        };
        if !pinned {
            return Err(DaemonError::PolicyDenied(format!(
                "review_source_not_pinned: {} does not pin {}",
                request.pin_ref, request.source_commit
            )));
        }
        let store = self.manager.store.lock().await;
        let assignment = store.manager_review_reserve_for_topology(&TopologyReviewRequest {
            execution_id: request.execution_id,
            attempt_id: request.attempt_id,
            node_id: request.node_id.clone(),
            execution_name: request.execution_name.clone(),
            project_id,
            epic_id,
            author_session_id: request.author_session_id,
            source_commit: request.source_commit.clone(),
            reviewer: rsi_common::harness_manager_v2::ManagerLaunchChoiceV2 {
                provider: request.reviewer.provider,
                model: request.reviewer.model.clone(),
                effort: Some(request.reviewer.effort.clone()),
            },
            query: request.query.clone(),
            previous_assignment: request.previous_assignment,
            extra_contributors: request.extra_contributors.clone(),
        })?;
        // The reserved row is the restart-safe retry owner: a transient
        // capacity or source race here is reconciled, never a second request.
        if let Err(error) = store.allocate_manager_review_assignment(assignment) {
            tracing::warn!(%assignment, %error, "topology review allocation deferred");
        }
        Ok(assignment)
    }

    /// #1715: an exhausted review's extra round under the manager ledger's own
    /// round policy (budget, closing-reviewer rule, contributor families).
    async fn review_extra_round(
        &self,
        request: crate::topology::review::ExtraRoundRequest,
    ) -> crate::topology::review::ExtraRound {
        use crate::store::manager_reviews::{TopologyExtraRound, TopologyExtraRoundQuery};
        use crate::topology::review::ExtraRound;
        let reviewer = rsi_common::harness_manager_v2::ManagerLaunchChoiceV2 {
            provider: request.reviewer.provider,
            model: request.reviewer.model.clone(),
            effort: Some(request.reviewer.effort.clone()),
        };
        let found = {
            let store = self.manager.store.lock().await;
            store.topology_review_extra_round(&TopologyExtraRoundQuery {
                execution_id: request.execution_id,
                node_id: &request.node_id,
                project_id: request.project_id,
                source_commit: &request.source_commit,
                reviewer: &reviewer,
                author_session_id: request.author_session_id,
                extra_contributors: &request.extra_contributors,
            })
        };
        match found {
            Ok(TopologyExtraRound::SameReviewer) => ExtraRound::Same,
            Ok(TopologyExtraRound::Closure(choice)) => {
                ExtraRound::Closure(rsi_common::types::ReviewerLaunch {
                    provider: choice.provider,
                    model: choice.model,
                    effort: choice.effort.unwrap_or(request.reviewer.effort),
                })
            }
            Ok(TopologyExtraRound::Unavailable(reason)) => ExtraRound::Unavailable(reason),
            // A read that fails is not a policy answer: say so, never offer
            // a round that cannot be checked.
            Err(error) => ExtraRound::Unavailable(format!("review_policy_unreadable: {error}")),
        }
    }

    async fn find_land_entry(&self, request: &crate::topology::land::LandRequest) -> Option<Uuid> {
        let store = self.manager.store.lock().await;
        match super::rolling_queue_verb::topology_land_entry(&store, request) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "topology land entry lookup failed");
                None
            }
        }
    }

    /// #1641 S2: the land node's enqueue on the daemon merge queue, on behalf
    /// of the execution's owning manager or Epic lead.
    async fn enqueue_land(
        &self,
        request: crate::topology::land::LandRequest,
    ) -> Result<crate::topology::land::LandEnqueue> {
        let enabled = self
            .manager
            .runtime_config
            .rolling_queue_enabled
            .load(Ordering::Relaxed);
        self.manager
            .agent_control()
            .enqueue_for_topology(&request, enabled)
            .await
    }

    async fn land_status(&self, entry_id: Uuid) -> crate::topology::land::LandStatus {
        use crate::topology::land::LandStatus;
        let store = self.manager.store.lock().await;
        // A read that fails is not a refusal: stay pending and re-derive on
        // the next tick.
        super::rolling_queue_verb::topology_land_status(&store, entry_id).unwrap_or_else(|error| {
            tracing::warn!(%entry_id, %error, "topology land status unreadable");
            LandStatus::Pending
        })
    }

    async fn record_oncall_acceptance(
        &self,
        acceptance: crate::topology::review::OncallAcceptance,
    ) -> Result<()> {
        let store = self.manager.store.lock().await;
        store.topology_record_oncall_acceptance(
            acceptance.assignment_id,
            &acceptance.commit,
            &crate::store::manager_reviews::TopologyOncallAcceptance {
                execution_id: acceptance.execution_id,
                attempt_id: acceptance.attempt_id,
                node_id: acceptance.node_id,
                decision_key: acceptance.decision_key,
            },
        )
    }

    async fn review_status(&self, assignment_id: Uuid) -> crate::topology::review::ReviewStatus {
        use crate::store::manager_reviews::TopologyReviewOutcome;
        use crate::topology::review::ReviewStatus;
        let outcome = self
            .manager
            .store
            .lock()
            .await
            .topology_review_outcome(assignment_id);
        match outcome {
            Ok(TopologyReviewOutcome::Pending) => ReviewStatus::Pending,
            Ok(TopologyReviewOutcome::Accepted {
                reviewed_commit,
                findings,
            }) => ReviewStatus::Accepted {
                findings,
                reviewed_commit,
            },
            Ok(TopologyReviewOutcome::ChangesRequested { findings }) => {
                ReviewStatus::ChangesRequested { findings }
            }
            Ok(TopologyReviewOutcome::Unsettled(reason)) => ReviewStatus::Unsettled(reason),
            // A read that fails is not a verdict: stay pending and re-derive
            // on the next tick rather than burning the node's one retry.
            Err(error) => {
                tracing::warn!(%assignment_id, %error, "topology review status unreadable");
                ReviewStatus::Pending
            }
        }
    }
}

struct UpdateSpec {
    node_id: Option<String>,
    status: WorkflowExecutionStatus,
    node_state: Option<WorkflowNodeExecutionState>,
    finished: bool,
    error: Option<String>,
    output_preview: Option<String>,
    mark_started: bool,
    final_output: Option<serde_json::Value>,
}

fn run_execution_task(
    executions: Arc<Mutex<WorkflowExecutionRegistry>>,
    event_bus: Arc<crate::bus::EventBus>,
    execution_id: Uuid,
    workflow: WorkflowDefinition,
    input: Option<serde_json::Value>,
    dry_run: bool,
    cancel_flag: Arc<AtomicBool>,
) -> Result<()> {
    let input_data = match input.clone() {
        Some(value) => match serde_json::from_value::<NodeData>(value) {
            Ok(parsed) => parsed,
            Err(error) => {
                let update = append_execution_update(
                    &executions,
                    execution_id,
                    UpdateSpec {
                        node_id: None,
                        status: WorkflowExecutionStatus::Failed,
                        node_state: None,
                        finished: true,
                        error: Some(format!("Invalid workflow input: {}", error)),
                        output_preview: None,
                        mark_started: false,
                        final_output: None,
                    },
                )?;
                event_bus.publish(DaemonEvent::GraphExecution { update });
                return Ok(());
            }
        },
        None => NodeData::new(),
    };

    let started_update = append_execution_update(
        &executions,
        execution_id,
        UpdateSpec {
            node_id: None,
            status: WorkflowExecutionStatus::Running,
            node_state: None,
            finished: false,
            error: None,
            output_preview: Some(format!("workflow accepted (dry_run={})", dry_run)),
            mark_started: true,
            final_output: None,
        },
    )?;
    event_bus.publish(DaemonEvent::GraphExecution {
        update: started_update,
    });

    let executions_for_updates = Arc::clone(&executions);
    let event_bus_for_updates = Arc::clone(&event_bus);
    let on_node_update = Arc::new(move |node_update: NodeExecutionUpdate| {
        let status = match node_update.state {
            NodeExecutionState::Running | NodeExecutionState::Succeeded => {
                WorkflowExecutionStatus::Running
            }
            NodeExecutionState::Failed => WorkflowExecutionStatus::Failed,
        };
        let node_state = match node_update.state {
            NodeExecutionState::Running => WorkflowNodeExecutionState::Running,
            NodeExecutionState::Succeeded => WorkflowNodeExecutionState::Succeeded,
            NodeExecutionState::Failed => WorkflowNodeExecutionState::Failed,
        };

        match append_execution_update(
            &executions_for_updates,
            execution_id,
            UpdateSpec {
                node_id: Some(node_update.node_id),
                status,
                node_state: Some(node_state),
                finished: false,
                error: None,
                output_preview: node_update.output_preview,
                mark_started: false,
                final_output: None,
            },
        ) {
            Ok(update) => {
                event_bus_for_updates.publish(DaemonEvent::GraphExecution { update });
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    execution_id = %execution_id,
                    "Failed to record node execution update"
                );
            }
        }
    });

    let result = crate::graph_exec::execute_workflow(
        &workflow,
        input_data,
        ExecutionHooks {
            cancel_flag: Some(Arc::clone(&cancel_flag)),
            on_node_update: Some(on_node_update),
        },
    )
    .map_err(|error| DaemonError::Process(format!("Workflow execution crashed: {}", error)))?;

    let final_status = if cancel_flag.load(Ordering::Relaxed) && !result.success {
        WorkflowExecutionStatus::Interrupted
    } else if result.success {
        WorkflowExecutionStatus::Succeeded
    } else {
        WorkflowExecutionStatus::Failed
    };

    let final_update = append_execution_update(
        &executions,
        execution_id,
        UpdateSpec {
            node_id: None,
            status: final_status,
            node_state: None,
            finished: true,
            error: result.error.clone(),
            output_preview: result.output.as_ref().and_then(preview_json_value),
            mark_started: false,
            final_output: result.output,
        },
    )?;
    event_bus.publish(DaemonEvent::GraphExecution {
        update: final_update,
    });

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_async_execution_task(
    session_manager: Arc<SessionManager>,
    executions: Arc<Mutex<WorkflowExecutionRegistry>>,
    event_bus: Arc<crate::bus::EventBus>,
    execution_id: Uuid,
    workflow_id: Uuid,
    workflow: WorkflowDefinition,
    input: Option<serde_json::Value>,
    cancel_flag: Arc<AtomicBool>,
    project_id: Option<Uuid>,
    parent_id: Option<Uuid>,
    custody_plan: crate::topology::custody::TopologyCustodyPlan,
    custody: Option<crate::topology::custody::TopologyCustody>,
) {
    // Parse input.
    let mut input_data = match input {
        Some(value) => match serde_json::from_value::<NodeData>(value) {
            Ok(parsed) => parsed,
            Err(error) => {
                let update = append_execution_update(
                    &executions,
                    execution_id,
                    UpdateSpec {
                        node_id: None,
                        status: WorkflowExecutionStatus::Failed,
                        node_state: None,
                        finished: true,
                        error: Some(format!("Invalid workflow input: {}", error)),
                        output_preview: None,
                        mark_started: false,
                        final_output: None,
                    },
                );
                if let Ok(update) = update {
                    event_bus.publish(DaemonEvent::GraphExecution { update });
                }
                return;
            }
        },
        None => NodeData::new(),
    };
    input_data.remove("base_commit");

    // Emit Running update.
    let started_update = append_execution_update(
        &executions,
        execution_id,
        UpdateSpec {
            node_id: None,
            status: WorkflowExecutionStatus::Running,
            node_state: None,
            finished: false,
            error: None,
            output_preview: Some("workflow executing (live)".to_string()),
            mark_started: true,
            final_output: None,
        },
    );
    if let Ok(update) = started_update {
        event_bus.publish(DaemonEvent::GraphExecution { update });
    }

    // Build node update callback (same pattern as existing run_execution_task).
    let executions_for_updates = Arc::clone(&executions);
    let event_bus_for_updates = Arc::clone(&event_bus);
    let on_node_update = Arc::new(move |node_update: NodeExecutionUpdate| {
        let status = match node_update.state {
            NodeExecutionState::Running | NodeExecutionState::Succeeded => {
                WorkflowExecutionStatus::Running
            }
            NodeExecutionState::Failed => WorkflowExecutionStatus::Failed,
        };
        let node_state = match node_update.state {
            NodeExecutionState::Running => WorkflowNodeExecutionState::Running,
            NodeExecutionState::Succeeded => WorkflowNodeExecutionState::Succeeded,
            NodeExecutionState::Failed => WorkflowNodeExecutionState::Failed,
        };

        match append_execution_update(
            &executions_for_updates,
            execution_id,
            UpdateSpec {
                node_id: Some(node_update.node_id),
                status,
                node_state: Some(node_state),
                finished: false,
                error: None,
                output_preview: node_update.output_preview,
                mark_started: false,
                final_output: None,
            },
        ) {
            Ok(update) => {
                event_bus_for_updates.publish(DaemonEvent::GraphExecution { update });
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    execution_id = %execution_id,
                    "Failed to record node execution update"
                );
            }
        }
    });

    let ctx = super::graph_runner::GraphRunnerContext {
        event_bus: Arc::clone(&event_bus),
        cancel_flag: Arc::clone(&cancel_flag),
        on_node_update,
        project_id,
        workflow_id,
        parent_id,
        is_topology: workflow.metadata.contains_key("source_topology_id"),
        custody: custody.expect("live executions resolve custody before launch"),
        custody_plan,
    };

    let result = super::graph_runner::run_graph_workflow(
        Arc::clone(&session_manager),
        &ctx,
        &workflow,
        input_data,
    )
    .await;
    if result.success
        && let Err(error) = ctx.custody.release_pins()
    {
        tracing::warn!(execution_id = %execution_id, %error, "topology pin cleanup failed");
    }

    // Determine final status.
    let final_status = if cancel_flag.load(Ordering::Relaxed) && !result.success {
        WorkflowExecutionStatus::Interrupted
    } else if result.success {
        WorkflowExecutionStatus::Succeeded
    } else {
        WorkflowExecutionStatus::Failed
    };

    let final_update = append_execution_update(
        &executions,
        execution_id,
        UpdateSpec {
            node_id: None,
            status: final_status,
            node_state: None,
            finished: true,
            error: result.error.clone(),
            output_preview: result.output.as_ref().and_then(preview_json_value),
            mark_started: false,
            final_output: result.output,
        },
    );
    if let Ok(update) = final_update {
        event_bus.publish(DaemonEvent::GraphExecution { update });
    }
}

fn append_execution_update(
    executions: &Arc<Mutex<WorkflowExecutionRegistry>>,
    execution_id: Uuid,
    spec: UpdateSpec,
) -> Result<GraphExecutionUpdate> {
    let mut executions = lock_workflow_executions(executions)?;
    let Some(entry) = executions.entries.get_mut(&execution_id) else {
        return Err(DaemonError::Store(format!(
            "Workflow execution not found: {}",
            execution_id
        )));
    };

    let updated_at = Utc::now();
    entry.snapshot.last_sequence += 1;
    entry.snapshot.status = spec.status;

    if spec.mark_started && entry.snapshot.started_at.is_none() {
        entry.snapshot.started_at = Some(updated_at);
    }
    if spec.finished {
        entry.snapshot.finished_at = Some(updated_at);
    }
    if let Some(output) = spec.final_output {
        entry.snapshot.output = Some(output);
    }
    if let Some(error) = spec.error.clone() {
        entry.snapshot.error = Some(error);
    }

    let update = GraphExecutionUpdate {
        execution_id,
        workflow_id: entry.snapshot.workflow_id,
        node_id: spec.node_id,
        sequence: entry.snapshot.last_sequence,
        status: spec.status,
        node_state: spec.node_state,
        finished: spec.finished,
        error: spec.error,
        output_preview: spec.output_preview,
        updated_at,
    };
    entry.snapshot.updates.push(update.clone());

    if spec.finished {
        prune_execution_registry(&mut executions, updated_at);
    }

    Ok(update)
}

fn lookup_execution(
    executions: &WorkflowExecutionRegistry,
    execution_id: Uuid,
) -> WorkflowExecutionLookup {
    if let Some(entry) = executions.entries.get(&execution_id) {
        return WorkflowExecutionLookup::Found {
            execution: entry.snapshot.clone(),
        };
    }

    if let Some(expired_at) = executions.expired.get(&execution_id) {
        return WorkflowExecutionLookup::Expired {
            execution_id,
            expired_at: *expired_at,
        };
    }

    WorkflowExecutionLookup::NotFound { execution_id }
}

fn prune_execution_registry(executions: &mut WorkflowExecutionRegistry, now: DateTime<Utc>) {
    let cutoff = now - Duration::hours(EXECUTION_RETENTION_TTL_HOURS);

    let mut completed: Vec<(Uuid, DateTime<Utc>)> = executions
        .entries
        .iter()
        .filter_map(|(execution_id, entry)| {
            entry
                .snapshot
                .finished_at
                .map(|finished_at| (*execution_id, finished_at))
        })
        .collect();

    let mut remove_ids: Vec<Uuid> = completed
        .iter()
        .filter_map(|(execution_id, finished_at)| (*finished_at < cutoff).then_some(*execution_id))
        .collect();

    if completed.len() > COMPLETED_EXECUTION_RETENTION_LIMIT {
        completed.sort_by_key(|(_, finished_at)| *finished_at);
        let overflow = completed.len() - COMPLETED_EXECUTION_RETENTION_LIMIT;
        remove_ids.extend(
            completed
                .into_iter()
                .take(overflow)
                .map(|(execution_id, _)| execution_id),
        );
    }

    remove_ids.sort_unstable();
    remove_ids.dedup();

    for execution_id in remove_ids {
        if executions.entries.remove(&execution_id).is_some() {
            executions.expired.insert(execution_id, now);
        }
    }

    executions
        .expired
        .retain(|_, expired_at| *expired_at >= cutoff);
    if executions.expired.len() > EXPIRED_EXECUTION_RETENTION_LIMIT {
        let mut expired: Vec<(Uuid, DateTime<Utc>)> = executions
            .expired
            .iter()
            .map(|(execution_id, expired_at)| (*execution_id, *expired_at))
            .collect();
        expired.sort_by_key(|(_, expired_at)| *expired_at);
        let overflow = expired.len() - EXPIRED_EXECUTION_RETENTION_LIMIT;
        for (execution_id, _) in expired.into_iter().take(overflow) {
            executions.expired.remove(&execution_id);
        }
    }
}

fn validation_summary(report: &WorkflowValidationReport) -> String {
    let blocking_messages: Vec<&str> = report
        .diagnostics
        .iter()
        .filter(|diag| diag.is_blocking())
        .map(|diag| diag.message.as_str())
        .take(3)
        .collect();

    if blocking_messages.is_empty() {
        return "workflow validation failed".to_string();
    }

    format!(
        "workflow validation failed ({} issues): {}",
        report.error_count(),
        blocking_messages.join("; ")
    )
}

fn lock_workflow_executions<'a>(
    executions: &'a Arc<Mutex<WorkflowExecutionRegistry>>,
) -> Result<MutexGuard<'a, WorkflowExecutionRegistry>> {
    executions
        .lock()
        .map_err(|_| DaemonError::Store("workflow execution lock poisoned".to_string()))
}

fn preview_json_value(value: &serde_json::Value) -> Option<String> {
    let json = serde_json::to_string(value).ok()?;
    const MAX_PREVIEW_CHARS: usize = 120;
    if json.chars().count() > MAX_PREVIEW_CHARS {
        let truncated: String = json.chars().take(MAX_PREVIEW_CHARS).collect();
        Some(format!("{}...", truncated))
    } else {
        Some(json)
    }
}

/// Longest question a snapshot carries (the ledger keeps the full text).
const RULING_QUESTION_CLIP: usize = 500;

/// Node ids with an attempt that has not settled, sorted and unique.
fn current_nodes(attempts: &[crate::topology::store::AttemptRow]) -> Vec<String> {
    use crate::topology::store::AttemptStatus::{Launching, Reserved, Running, Waiting};
    let mut nodes: Vec<String> = attempts
        .iter()
        .filter(|attempt| matches!(attempt.status, Reserved | Launching | Running | Waiting))
        .map(|attempt| attempt.node_id.clone())
        .collect();
    nodes.sort();
    nodes.dedup();
    nodes
}

/// The `topology:<execution>:` decisions among `records`, oldest first and
/// capped at the newest [`MAX_SNAPSHOT_RULINGS`]. The answer is a digest, never
/// the text.
fn ruling_views(
    execution_id: Uuid,
    records: &[crate::store::harness_manager_v2::ManagerRecordV2],
) -> Vec<TopologyRulingView> {
    use sha2::Digest;
    let prefix = format!(
        "{}{execution_id}:",
        crate::store::manager_decisions::TOPOLOGY_DECISION_PREFIX
    );
    let mut own: Vec<_> = records
        .iter()
        .filter(|record| record.key.starts_with(&prefix))
        .collect();
    own.sort_by(|a, b| (&a.created_at, &a.key).cmp(&(&b.created_at, &b.key)));
    let skip = own.len().saturating_sub(MAX_SNAPSHOT_RULINGS);
    own.into_iter()
        .skip(skip)
        .map(|record| {
            let payload = &record.payload;
            let mut question = payload["question"].as_str().unwrap_or_default().to_owned();
            if question.len() > RULING_QUESTION_CLIP {
                let mut end = RULING_QUESTION_CLIP;
                while !question.is_char_boundary(end) {
                    end -= 1;
                }
                question.truncate(end);
                question.push('…');
            }
            TopologyRulingView {
                node_id: payload["asked_by"]["node_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                decision_key: record.key.clone(),
                question,
                status: payload["status"].as_str().unwrap_or_default().to_owned(),
                answered_by: payload["answered_by"]["kind"].as_str().map(str::to_owned),
                answer_digest: payload["answer"]
                    .as_str()
                    .map(|answer| format!("sha256:{}", hex::encode(sha2::Sha256::digest(answer)))),
            }
        })
        .collect()
}

/// #1641 S6a: add the run-view fields (current nodes, on-call seat, rulings)
/// to a durable execution snapshot. All three are re-derived from rows, so a
/// restart changes nothing; a final execution has no current nodes and no
/// on-call seat but keeps its rulings.
pub(crate) fn attach_run_view(
    store: &crate::store::Store,
    snapshot: &mut WorkflowExecutionSnapshot,
) -> Result<()> {
    use crate::topology::{oncall, store as rows};
    let Some(execution) = rows::load_execution(store, snapshot.execution_id)? else {
        return Ok(());
    };
    if !execution.status.is_final() {
        snapshot.current_nodes = current_nodes(&rows::load_attempts(store, execution.id)?);
        let named = oncall::seat_of(execution.input.as_ref());
        snapshot.on_call = Some(
            match oncall::resolve(store, execution.project_id, &named)? {
                oncall::OnCall::Live { seat, session_id } => TopologyOnCallView {
                    seat,
                    session_id: Some(session_id),
                    live: true,
                },
                oncall::OnCall::Unavailable { .. } => TopologyOnCallView {
                    seat: named,
                    session_id: None,
                    live: false,
                },
            },
        );
    }
    if let (Some(project), Some(epic)) = (execution.project_id, execution.epic_id)
        && let Some(config) = store
            .get_harness_manager(project)?
            .filter(|config| config.epic_ids.contains(&epic))
    {
        snapshot.rulings = ruling_views(
            execution.id,
            &store.manager_v2_records_of_kind(&config, "decision")?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1728: the restart resume of a node session is fenced, so it cannot
    /// run beside a continuation of the same session that already owns it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn restart_resume_refuses_busy_beside_a_competing_continuation() {
        use crate::bus::EventBus;
        use crate::config::{Config, RuntimeConfig};
        use crate::session::TrackedSession;
        use crate::store::Store;
        use crate::topology::executor::NodeEffects;

        let state = crate::test_support::disk_backed_tempdir("restart-resume-busy");
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open(&state.path().join("rsi.db")).unwrap(),
                false,
                state.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                state.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let mut node =
            rsid_store::test_support::test_session(Uuid::new_v4(), state.path().to_path_buf());
        node.status = rsi_common::types::SessionStatus::Interrupted;
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&node).unwrap();
            store.publish_startup_ordinary(node.id).unwrap();
        }
        // The operator's continuation already runs the session.
        let mut running = node.clone();
        running.status = rsi_common::types::SessionStatus::Running;
        manager
            .active
            .write()
            .await
            .insert(node.id, TrackedSession::new_for_test(running));
        let events_before = manager
            .store
            .lock()
            .await
            .load_events(node.id)
            .unwrap()
            .len();

        let effects = SessionNodeEffects {
            manager: Arc::clone(&manager),
        };
        let refused = effects
            .resume_cut_session(
                node.id,
                "resume".into(),
                effects.session(node.id).await.unwrap().restart_cut_fence,
            )
            .await
            .unwrap_err();
        assert_eq!(
            crate::store::manager_actions::fence::continuation_fence_code(&refused),
            Some(crate::store::manager_actions::fence::CONTINUATION_TARGET_BUSY)
        );
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .load_events(node.id)
                .unwrap()
                .len(),
            events_before,
            "the refused resume delivered nothing"
        );
    }

    /// Admit a real same-UUID continuation through the production lifecycle,
    /// then finish its scripted provider through the production monitor.
    async fn competing_restart_turn(manager: &Arc<SessionManager>, id: Uuid) -> Uuid {
        let process = super::super::launch::install_controller_candidate_test_process(id);
        manager
            .continue_session_operator(id, "operator continuation".into())
            .await
            .unwrap();
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
        // The continuation returns after installing the provider, while its
        // monitor bootstrap binds the admitted invocation asynchronously.
        let invocation = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let invocation = manager
                    .store
                    .lock()
                    .await
                    .session_model_invocation_id(id)
                    .unwrap();
                if let Some(invocation) = invocation {
                    let store = manager.store.lock().await;
                    let record = store
                        .load_model_invocation_record(invocation)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        record.admission_status,
                        rsi_common::model_control::AdmissionStatus::Admitted
                    );
                    assert_eq!(record.raw_status, "running");
                    break invocation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real continuation bound its admitted invocation");
        assert!(manager.active.read().await.contains_key(&id));
        process.alive.store(false, Ordering::SeqCst);
        super::super::launch::send_controller_candidate_test_event(
            id,
            crate::claude::StreamEvent {
                event_type: "result".into(),
                data: serde_json::json!({"subtype":"success", "result":"operator turn finished",
                "is_error":false, "num_turns":1, "total_cost_usd":0.0}),
            },
        )
        .await;
        super::super::launch::drop_controller_candidate_test_stream(id);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let terminal = {
                    let store = manager.store.lock().await;
                    store.get_session(id).unwrap().unwrap().status
                        == rsi_common::types::SessionStatus::Completed
                        && store
                            .load_model_invocation_record(invocation)
                            .unwrap()
                            .unwrap()
                            .raw_status
                            == "completed"
                };
                if terminal && !manager.active.read().await.contains_key(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real competing continuation terminalized");
        invocation
    }

    fn restart_resume_repo(path: &std::path::Path) -> String {
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Topology Test",
                    "-c",
                    "user.email=topology@test.invalid",
                ])
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        git(&["add", "base.txt"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["rev-parse", "HEAD"])
    }

    fn resumable_restart_node(id: Uuid, path: &std::path::Path) -> rsi_common::types::Session {
        let mut node = rsid_store::test_support::test_session(id, path.to_path_buf());
        node.provider = rsi_common::types::SessionProvider::Claude;
        node.session_kind = rsi_common::types::SessionKind::Standard;
        node.project_id = None;
        node.max_retries = Some(0);
        node.status = rsi_common::types::SessionStatus::Interrupted;
        node.stop_reason = Some("interrupted:daemon_restart".into());
        node
    }

    /// #1728: an operator continuation of the same session that starts AND
    /// completes after the executor's restart-cut observation but before the
    /// resume takes the guard leaves nothing active, yet the stale resume is
    /// refused on the observed turn cursor and no second invocation starts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn restart_resume_refuses_a_stale_turn_after_a_competing_continuation_completes() {
        use crate::bus::EventBus;
        use crate::config::{Config, RuntimeConfig};
        use crate::store::Store;
        use crate::topology::executor::NodeEffects;

        let state = crate::test_support::disk_backed_tempdir("restart-resume-stale-turn");
        restart_resume_repo(state.path());
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open(&state.path().join("rsi.db")).unwrap(),
                false,
                state.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                state.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let node = resumable_restart_node(Uuid::new_v4(), state.path());
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&node).unwrap();
            store.publish_startup_ordinary(node.id).unwrap();
        }
        let effects = SessionNodeEffects {
            manager: Arc::clone(&manager),
        };
        // The executor's classifying observation: the session is idle and cut.
        let observed = effects.session(node.id).await.unwrap();
        assert_eq!(
            observed
                .restart_cut_fence
                .as_ref()
                .map(|fence| fence.event_sequence),
            Some(-1),
            "no turn recorded yet"
        );

        // Recovery pauses after the fence capture, before the spawn guard.
        let (reached, resume) =
            crate::session::lifecycle::install_continuation_pause_for_test(node.id);
        let recovery = {
            let effects = SessionNodeEffects {
                manager: Arc::clone(&manager),
            };
            let node_id = node.id;
            let cursor = observed.restart_cut_fence;
            tokio::spawn(async move {
                effects
                    .resume_cut_session(node_id, "resume".into(), cursor)
                    .await
            })
        };
        reached.await.unwrap();
        let invocation = competing_restart_turn(&manager, node.id).await;
        let events_after_turn = manager
            .store
            .lock()
            .await
            .load_events(node.id)
            .unwrap()
            .len();
        resume.send(()).unwrap();

        let refused = recovery.await.unwrap().unwrap_err();
        assert_eq!(
            crate::store::manager_actions::fence::continuation_fence_code(&refused),
            Some(crate::store::manager_actions::fence::CONTINUATION_TURN_CHANGED)
        );
        assert!(
            !manager.active.read().await.contains_key(&node.id),
            "the stale resume admitted no second invocation"
        );
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .load_events(node.id)
                .unwrap()
                .len(),
            events_after_turn,
            "the refused resume delivered nothing"
        );
        let store = manager.store.lock().await;
        assert_eq!(
            store.session_model_invocation_id(node.id).unwrap(),
            Some(invocation)
        );
        let admitted: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE session_id=?1 AND admission_status='admitted'",
            [node.id.to_string()], |row| row.get(0),
        ).unwrap();
        assert_eq!(admitted, 1);
    }

    /// #1728 round 3: the real operator turn starts and terminalizes at the
    /// old classification -> cursor boundary. Recovery must defer and follow
    /// the next observation, without preserving or admitting another turn.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn restart_observation_defers_after_a_real_competing_turn_in_the_read_gap() {
        use crate::config::{Config, RuntimeConfig};
        use crate::topology::{
            executor::{Executor, Step},
            store as rows,
        };

        let state = crate::test_support::disk_backed_tempdir("restart-observation-race-");
        let base = restart_resume_repo(state.path());
        let runtime = RuntimeConfig::from_config(&Config::from_env());
        runtime
            .topology_executor_enabled
            .store(true, Ordering::Relaxed);
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                crate::store::Store::open(&state.path().join("rsi.db")).unwrap(),
                false,
                state.path().join("daemon.sock"),
                None,
                Vec::new(),
                runtime,
                state.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let definition = WorkflowDefinition {
            version: "1.0".into(),
            name: "restart race".into(),
            description: String::new(),
            nodes: vec![rsi_graph::format::NodeDef::action("A", "A")],
            edges: Vec::new(),
            metadata: Default::default(),
        };
        let execution = Uuid::new_v4();
        let node = {
            let mut store = manager.store.lock().await;
            rows::insert_execution(
                &store,
                &rows::NewExecution {
                    id: execution,
                    topology_id: None,
                    workflow_id: Uuid::new_v4(),
                    custody_plan: super::super::graph_runner::plan_workflow_custody(&definition)
                        .unwrap(),
                    definition,
                    project_id: None,
                    parent_session_id: None,
                    repo_root: state.path().to_path_buf(),
                    base_commit: base.clone(),
                    input: None,
                    requester: None,
                    owner: None,
                },
            )
            .unwrap();
            rows::transition_execution(
                &store,
                execution,
                &[rows::ExecutionStatus::Accepted],
                rows::ExecutionStatus::Running,
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();
            rows::reserve_attempts(
                &store,
                execution,
                &[rows::NewAttempt {
                    node_id: "A".into(),
                    iteration: 0,
                    attempt_no: 1,
                    base_commit: base,
                    query: "continue task".into(),
                    node_kind: "session",
                    catalog_op: None,
                    effect_class: None,
                }],
            )
            .unwrap();
            let attempt = rows::load_attempts(&store, execution).unwrap().remove(0);
            rows::mark_launching(&store, attempt.id, Uuid::new_v4()).unwrap();
            rows::mark_running(&store, execution, &attempt, None).unwrap();
            let node = resumable_restart_node(attempt.session_id, state.path());
            store.insert_session(&node).unwrap();
            store.publish_startup_ordinary(node.id).unwrap();
            node
        };
        let effects = Arc::new(SessionNodeEffects {
            manager: Arc::clone(&manager),
        });
        let boot_id = Uuid::new_v4();
        let executor = Arc::new(Executor::new(
            Arc::clone(&manager.store),
            effects,
            boot_id,
            Arc::default(),
        ));
        let (reached, resume) = install_restart_observation_pause(node.id);
        let recovery = {
            let executor = Arc::clone(&executor);
            tokio::spawn(async move { executor.advance(execution).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), reached)
            .await
            .unwrap()
            .unwrap();
        let invocation = competing_restart_turn(&manager, node.id).await;
        let events_after_turn = manager
            .store
            .lock()
            .await
            .load_events(node.id)
            .unwrap()
            .len();
        // A broken observation must stay inside the scripted provider seam:
        // this also counts a second provider start directly in the regression.
        let stale_provider =
            super::super::launch::install_controller_candidate_test_process(node.id);
        resume.send(()).unwrap();
        let step = recovery.await.unwrap().unwrap();
        super::super::launch::drop_controller_candidate_test_process(node.id);
        stale_provider.alive.store(false, Ordering::SeqCst);
        super::super::launch::drop_controller_candidate_test_stream(node.id);
        assert_eq!(
            stale_provider.productive_start_count.load(Ordering::SeqCst),
            0,
            "stale recovery started a second provider"
        );
        assert_eq!(step, Step::Wait);
        {
            let store = manager.store.lock().await;
            let attempt = rows::load_attempts(&store, execution).unwrap().remove(0);
            assert_eq!(attempt.status, rows::AttemptStatus::Running);
            assert_eq!(attempt.failure_class, None);
            assert_eq!(attempt.preserved_commit, None);
            assert!(!rows::node_resume_tried(&store, attempt.id, boot_id).unwrap());
            assert_eq!(
                store.session_model_invocation_id(node.id).unwrap(),
                Some(invocation)
            );
            assert_eq!(store.load_events(node.id).unwrap().len(), events_after_turn);
            let admitted: i64 = store.conn.query_row(
                "SELECT COUNT(*) FROM model_invocations WHERE session_id=?1 AND admission_status='admitted'",
                [node.id.to_string()], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                admitted, 1,
                "only the operator's real continuation was admitted"
            );
        }
        // Re-observe the completed competing turn. This fixture is an
        // ordinary session (no topology sandbox), so its eventual graph
        // settlement is outside this restart-observation regression.
        use crate::topology::executor::NodeEffects;
        let fresh = SessionNodeEffects {
            manager: Arc::clone(&manager),
        }
        .session(node.id)
        .await
        .unwrap();
        assert_eq!(fresh.status, rsi_common::types::SessionStatus::Completed);
        assert!(!fresh.cut_by_restart());
        let store = manager.store.lock().await;
        let (_, fresh_cursor) = store.restart_cut_observation(node.id).unwrap().unwrap();
        assert_eq!(fresh_cursor.invocation_id, Some(invocation));
        assert!(fresh_cursor.event_sequence >= 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn t1_a3_invalid_node_working_dir_refuses_before_launch() {
        use crate::bus::EventBus;
        use crate::config::{Config, RuntimeConfig};
        use crate::store::Store;
        use rsi_graph::format::NodeDef;

        let repo = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.path().join("file.txt"), "base").unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(result.status.success());
            String::from_utf8(result.stdout).unwrap().trim().to_owned()
        };
        git(&["add", "file.txt"]);
        git(&[
            "-c",
            "user.name=Topology Test",
            "-c",
            "user.email=topology@test.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ]);
        let base = git(&["rev-parse", "HEAD"]);
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open(&state.path().join("rsi.db")).unwrap(),
                false,
                state.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                state.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let mut node = NodeDef::action("A", "A");
        node.instructions = "do work".into();
        node.working_dir = Some(other.path().to_path_buf());
        let workflow = WorkflowDefinition {
            version: "1.0".into(),
            name: "invalid-node-dir".into(),
            description: String::new(),
            nodes: vec![node],
            edges: vec![],
            metadata: Default::default(),
        };
        let error = SessionManager::execute_workflow_live(
            manager,
            Uuid::new_v4(),
            workflow,
            Some(serde_json::json!({"base_commit": base})),
            false,
            None,
            Some(repo.path().to_path_buf()),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DaemonError::InvalidParam(_)));
        assert_eq!(
            std::fs::read_dir(state.path().join("sandboxes"))
                .unwrap()
                .count(),
            0,
            "invalid node working_dir must not allocate a sandbox"
        );
    }

    /// T2-A11 (R3-3): `ExecuteTopology`'s live path plans whole-workflow
    /// custody before any durable row: an unresolved fan-in, a non-ancestor
    /// `custody.from` and two back-edges into one node are all refused with
    /// zero execution, attempt, invocation and session rows and no sandbox.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    #[allow(clippy::too_many_lines, clippy::unwrap_used)]
    async fn t2_a11_unresolved_fanin_refused_zero_launch() {
        use crate::bus::EventBus;
        use crate::config::{Config, RuntimeConfig};
        use crate::store::Store;
        use rsi_graph::data::Value as GraphValue;
        use rsi_graph::format::{EdgeDef, NodeDef};

        let repo = tempfile::TempDir::new().unwrap();
        let state = tempfile::TempDir::new().unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .args(["-c", "user.name=T", "-c", "user.email=t@test.invalid"])
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(result.status.success());
            String::from_utf8(result.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("file.txt"), "base").unwrap();
        git(&["add", "file.txt"]);
        git(&["commit", "-q", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        let runtime = RuntimeConfig::from_config(&Config::from_env());
        runtime
            .topology_executor_enabled
            .store(true, Ordering::Relaxed);
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open(&state.path().join("rsi.db")).unwrap(),
                false,
                state.path().join("daemon.sock"),
                None,
                Vec::new(),
                runtime,
                state.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let node = |id: &str| {
            let mut node = NodeDef::action(id, id);
            node.instructions = format!("do {id}");
            node
        };
        let definition = |nodes: Vec<NodeDef>, edges: &[(&str, &str)]| WorkflowDefinition {
            version: "1.0".into(),
            name: "refused".into(),
            description: String::new(),
            nodes,
            edges: edges
                .iter()
                .map(|(from, to)| EdgeDef::new(*from, *to))
                .collect(),
            metadata: std::collections::BTreeMap::default(),
        };
        let fan_in = definition(
            vec![node("A"), node("B"), node("J")],
            &[("A", "J"), ("B", "J")],
        );
        let mut stranger = node("B");
        stranger.tags.push("custody.from=node:C".into());
        let non_ancestor = definition(vec![node("A"), stranger, node("C")], &[("A", "B")]);
        let mut two_back_edges = definition(
            vec![node("A"), node("B"), node("C")],
            &[("A", "B"), ("B", "C"), ("C", "A"), ("C", "B")],
        );
        two_back_edges.metadata.insert(
            "loop_edges".into(),
            GraphValue::String(r#"[{"from":"C","to":"A"},{"from":"C","to":"B"}]"#.into()),
        );
        two_back_edges.metadata.insert(
            "scc_regions".into(),
            GraphValue::String(r#"[["A","B","C"]]"#.into()),
        );

        for workflow in [fan_in, non_ancestor, two_back_edges] {
            let error = SessionManager::execute_workflow_live(
                Arc::clone(&manager),
                Uuid::new_v4(),
                workflow,
                Some(serde_json::json!({ "base_commit": base })),
                false,
                None,
                Some(repo.path().to_path_buf()),
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, DaemonError::InvalidParam(_)), "{error}");
        }
        let store = manager.store.lock().await;
        for table in [
            "topology_executions",
            "topology_node_attempts",
            "topology_events",
            "model_invocations",
            "sessions",
        ] {
            let rows: i64 = store
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(rows, 0, "{table} must stay empty");
        }
        assert!(
            !state.path().join("sandboxes").exists()
                || std::fs::read_dir(state.path().join("sandboxes"))
                    .unwrap()
                    .next()
                    .is_none()
        );
    }

    fn completed_entry(
        execution_id: Uuid,
        workflow_id: Uuid,
        finished_at: DateTime<Utc>,
    ) -> TrackedWorkflowExecution {
        TrackedWorkflowExecution {
            snapshot: WorkflowExecutionSnapshot {
                execution_id,
                workflow_id,
                workflow_name: "workflow".to_string(),
                status: WorkflowExecutionStatus::Succeeded,
                accepted_at: finished_at,
                started_at: Some(finished_at),
                finished_at: Some(finished_at),
                dry_run: false,
                input: None,
                output: None,
                error: None,
                last_sequence: 1,
                row_version: None,
                blocked_attempt_id: None,
                blocked_reason: None,
                waiting: None,
                current_nodes: Vec::new(),
                on_call: None,
                rulings: Vec::new(),
                updates: Vec::new(),
            },
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn prune_registry_marks_old_completed_execution_as_expired() {
        let execution_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        let now = Utc::now();
        let mut registry = WorkflowExecutionRegistry::default();
        registry.entries.insert(
            execution_id,
            completed_entry(execution_id, workflow_id, now - Duration::hours(25)),
        );

        prune_execution_registry(&mut registry, now);

        assert!(!registry.entries.contains_key(&execution_id));
        assert!(registry.expired.contains_key(&execution_id));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn lookup_execution_distinguishes_expired_from_not_found() {
        let expired_execution_id = Uuid::new_v4();
        let now = Utc::now();
        let mut registry = WorkflowExecutionRegistry::default();
        registry.expired.insert(expired_execution_id, now);

        match lookup_execution(&registry, expired_execution_id) {
            WorkflowExecutionLookup::Expired { execution_id, .. } => {
                assert_eq!(execution_id, expired_execution_id);
            }
            _ => panic!("expected expired lookup"),
        }

        match lookup_execution(&registry, Uuid::new_v4()) {
            WorkflowExecutionLookup::NotFound { .. } => {}
            _ => panic!("expected not_found lookup"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn prune_registry_enforces_completed_execution_limit() {
        let workflow_id = Uuid::new_v4();
        let now = Utc::now();
        let mut registry = WorkflowExecutionRegistry::default();

        for offset in 0..=COMPLETED_EXECUTION_RETENTION_LIMIT {
            let execution_id = Uuid::new_v4();
            let finished_at = now - Duration::minutes(offset as i64);
            registry.entries.insert(
                execution_id,
                completed_entry(execution_id, workflow_id, finished_at),
            );
        }

        prune_execution_registry(&mut registry, now);

        assert_eq!(registry.entries.len(), COMPLETED_EXECUTION_RETENTION_LIMIT);
        assert_eq!(registry.expired.len(), 1);
    }

    /// #1641 S6a: the run view lists the nodes still in flight and the
    /// execution's rulings (newest 32, digests only), and ignores other runs'.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn snapshot_lists_rulings_and_current_node() {
        use crate::store::harness_manager_v2::ManagerRecordV2;
        use crate::topology::store::{AttemptRow, AttemptStatus};
        use serde_json::json;

        let attempt = |node: &str, status| AttemptRow {
            id: Uuid::new_v4(),
            node_id: node.into(),
            iteration: 0,
            attempt_no: 1,
            status,
            dedup_key: String::new(),
            session_id: Uuid::new_v4(),
            boot_id: None,
            sandbox_root: None,
            base_commit: String::new(),
            result_commit: None,
            input: json!({}),
            output: None,
            failure_class: None,
            error: None,
            resolution: None,
            preserved_ref: None,
            preserved_commit: None,
            started_at: None,
            node_kind: "session".into(),
            pre_head: None,
            process_group_id: None,
            review_assignment_id: None,
            land_entry_id: None,
        };
        let attempts = [
            attempt("implement", AttemptStatus::Succeeded),
            attempt("review", AttemptStatus::Waiting),
            attempt("review", AttemptStatus::Reserved),
            attempt("fix", AttemptStatus::Cancelled),
        ];
        assert_eq!(current_nodes(&attempts), vec!["review".to_string()]);

        let run = Uuid::new_v4();
        let record = |key: String, n: usize, payload: serde_json::Value| ManagerRecordV2 {
            kind: "decision".into(),
            key,
            epic_id: None,
            row_version: 1,
            payload,
            archived: false,
            created_at: format!("2026-10-08T00:00:{n:02}Z"),
            updated_at: String::new(),
        };
        let secret = "use the staging token abc123";
        let mut records = vec![
            record(
                format!("topology:{run}:implement:0:1"),
                1,
                json!({
                    "question": "Keep the field?", "status": "answered", "answer": secret,
                    "asked_by": {"node_id": "implement"}, "answered_by": {"kind": "manager"},
                }),
            ),
            record(
                format!("topology:{}:implement:0:1", Uuid::new_v4()),
                2,
                json!({"question": "another run", "status": "pending"}),
            ),
            record(
                "question:not-a-topology-key".into(),
                3,
                json!({"question": "ordinary", "status": "pending"}),
            ),
        ];
        let views = ruling_views(run, &records);
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(view.node_id, "implement");
        assert_eq!(view.decision_key, format!("topology:{run}:implement:0:1"));
        assert_eq!(view.question, "Keep the field?");
        assert_eq!(view.status, "answered");
        assert_eq!(view.answered_by.as_deref(), Some("manager"));
        let digest = view.answer_digest.as_deref().unwrap();
        assert!(digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64);
        assert!(!serde_json::to_string(&views).unwrap().contains("abc123"));

        // A pending question has no answer yet; the cap keeps the newest.
        for n in 0..(MAX_SNAPSHOT_RULINGS + 5) {
            records.push(record(
                format!("topology:{run}:review:0:{n}"),
                10 + n,
                json!({"question": format!("q{n}"), "status": "pending"}),
            ));
        }
        let views = ruling_views(run, &records);
        assert_eq!(views.len(), MAX_SNAPSHOT_RULINGS);
        assert_eq!(
            views.last().unwrap().question,
            format!("q{}", MAX_SNAPSHOT_RULINGS + 4)
        );
        assert!(views.last().unwrap().answer_digest.is_none());
        assert_eq!(views.last().unwrap().answered_by, None);
    }

    /// #1417: the production effects forward the host-load hold for a manager's
    /// or Epic lead's execution only. An operator-requested execution is never
    /// held, and a held node is released by the load dropping.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn host_load_holds_only_agent_requested_topology_launches() {
        use crate::bus::EventBus;
        use crate::config::{Config, RuntimeConfig};
        use crate::host_load::LoadReading;
        use crate::store::Store;
        use crate::topology::executor::NodeEffects;
        use crate::topology::store::{AttemptRow, AttemptStatus};

        let state = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(EventBus::new(16)),
                Store::open(&state.path().join("rsi.db")).unwrap(),
                false,
                state.path().join("daemon.sock"),
                None,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                state.path().join("sandboxes"),
            )
            .unwrap(),
        );
        let load = Arc::new(std::sync::Mutex::new(75.0_f64));
        let shared = Arc::clone(&load);
        manager.host_load().set_source(Arc::new(move || {
            LoadReading::Load1(*shared.lock().unwrap())
        }));
        let effects = SessionNodeEffects {
            manager: Arc::clone(&manager),
        };
        let attempt = |node: &str| AttemptRow {
            id: Uuid::new_v4(),
            node_id: node.into(),
            iteration: 0,
            attempt_no: 1,
            status: AttemptStatus::Reserved,
            dedup_key: format!("topology.node:test:{node}:0:1"),
            session_id: Uuid::new_v4(),
            boot_id: None,
            sandbox_root: None,
            base_commit: "0".repeat(40),
            result_commit: None,
            input: serde_json::json!({}),
            output: None,
            failure_class: None,
            error: None,
            resolution: None,
            preserved_ref: None,
            preserved_commit: None,
            started_at: None,
            node_kind: "session".into(),
            pre_head: None,
            process_group_id: None,
            review_assignment_id: None,
            land_entry_id: None,
        };
        let (older, younger) = (attempt("older"), attempt("younger"));
        let now = Utc::now();
        let (older_since, younger_since) = (now - Duration::minutes(5), now);

        assert!(effects.launch_held(true, older_since, &older));
        assert!(effects.launch_held(true, younger_since, &younger));
        assert!(
            !effects.launch_held(false, younger_since, &younger),
            "an operator-requested execution is never held"
        );
        let status = manager.host_load().status(&[]);
        assert!(status.holding);
        assert_eq!(
            status
                .held
                .iter()
                .map(|item| item.session_id)
                .collect::<Vec<_>>(),
            vec![Some(older.session_id), Some(younger.session_id)],
            "held nodes are listed oldest first"
        );

        *load.lock().unwrap() = 10.0;
        assert!(
            effects.launch_held(true, younger_since, &younger),
            "the younger node does not overtake the older one"
        );
        assert!(!effects.launch_held(true, older_since, &older));
        assert!(!effects.launch_held(true, younger_since, &younger));
        assert!(manager.host_load().status(&[]).held.is_empty());
    }
}
