//! Async workflow execution tracking for graph runs.

use super::SessionManager;
use crate::bus::DaemonEvent;
use crate::error::{DaemonError, Result};
use crate::graph_exec::{ExecutionHooks, NodeExecutionState, NodeExecutionUpdate};
use chrono::{DateTime, Duration, Utc};
use rsi_common::rpc::{ExecuteWorkflowResponse, InterruptWorkflowExecutionResponse};
use rsi_common::types::{
    GraphExecutionUpdate, WorkflowExecutionLookup, WorkflowExecutionSnapshot,
    WorkflowExecutionStatus, WorkflowNodeExecutionState, WorkflowValidationReport,
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
        let validation = rsi_graph::validate_executable_workflow(&workflow);
        if validation.has_errors() {
            return Err(DaemonError::InvalidParam(validation_summary(&validation)));
        }
        let steps = crate::topology::steps::validate_workflow(&workflow)
            .map_err(DaemonError::InvalidParam)?;
        if !dry_run && steps.has_typed_effects() && !session_manager.topology_executor_enabled() {
            return Err(DaemonError::InvalidParam(
                "command and gate nodes need the durable topology executor (topology_executor_enabled)"
                    .into(),
            ));
        }

        let custody_base = if dry_run {
            None
        } else {
            let repo = working_dir.clone().unwrap_or(std::env::current_dir()?);
            if session_manager
                .store
                .lock()
                .await
                .path_is_inside_live_custody_root(&repo)?
            {
                return Err(DaemonError::InvalidParam(
                    "execution repository is an active sandbox".into(),
                ));
            }
            let explicit_base = input
                .as_ref()
                .and_then(|value| value.get("base_commit"))
                .and_then(serde_json::Value::as_str);
            let (root, commit) =
                crate::topology::custody::resolve_execution_base(&repo, explicit_base).await?;
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
                if let Some((node, krate)) =
                    crates.iter().find(|(_, krate)| !members.contains(*krate))
                {
                    return Err(DaemonError::InvalidParam(format!(
                        "node {node}: crate {krate} is not a workspace member"
                    )));
                }
            }
            Some((root, commit))
        };
        let custody_plan = if dry_run {
            None
        } else {
            Some(super::graph_runner::plan_workflow_custody(&workflow)?)
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
        Ok(
            crate::topology::store::execution_snapshot(&store, execution_id)?
                .map_or(lookup, |execution| WorkflowExecutionLookup::Found {
                    execution,
                }),
        )
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

    fn drive_topology_execution(
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

impl crate::topology::executor::NodeEffects for SessionNodeEffects {
    fn enabled(&self) -> bool {
        self.manager.topology_executor_enabled()
    }

    async fn launch(&self, request: crate::topology::executor::LaunchRequest) -> Result<Uuid> {
        self.manager
            .launch_session_with_retry_admission(
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
        self.manager.get_session(session_id).await.map(|session| {
            crate::topology::executor::SessionObservation {
                status: session.status,
                sandbox_root: session.sandbox_root,
            }
        })
    }

    async fn output(&self, session_id: Uuid) -> Result<NodeData> {
        let events = self.manager.get_conversation(session_id).await?;
        Ok(super::graph_runner::extract_session_output(&events))
    }

    async fn interrupt(&self, session_id: Uuid) {
        if let Err(error) = self.manager.interrupt_session(session_id).await {
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

    async fn allocate_command_sandbox(
        &self,
        session_id: Uuid,
        fork: crate::topology::custody::TopologyForkSource,
    ) -> Result<std::path::PathBuf> {
        let allocator = Arc::clone(&self.manager.sandbox_allocator);
        tokio::task::spawn_blocking(move || {
            crate::topology::catalog::allocate_or_adopt(&allocator, session_id, &fork)
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

    fn kill_stale_group(&self, pgid: i32) {
        crate::topology::catalog::kill_group(pgid);
    }

    fn build_node_cap(&self) -> u32 {
        self.manager
            .runtime_config
            .topology_max_concurrent_build_nodes
            .load(Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;

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
                updates: Vec::new(),
            },
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

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
}
