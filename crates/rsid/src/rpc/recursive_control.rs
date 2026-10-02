use super::*;

pub(super) const MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS: u32 = 10_000;

pub(super) const MAX_RECURSIVE_LIVE_SCHEDULER_RPC_STEPS: u32 = 10_000;

pub(super) fn validate_recursive_live_scheduler_params(
    params: &RunRecursiveLiveSchedulerParams,
) -> Result<()> {
    if params.max_steps == 0 {
        return Err(DaemonError::InvalidParam(
            "Invalid params: max_steps must be positive".to_string(),
        ));
    }
    if params.max_steps > MAX_RECURSIVE_LIVE_SCHEDULER_RPC_STEPS {
        return Err(DaemonError::InvalidParam(format!(
            "Invalid params: max_steps must be <= {MAX_RECURSIVE_LIVE_SCHEDULER_RPC_STEPS}"
        )));
    }
    validate_recursive_rpc_optional_nonempty("operator", params.operator.as_deref())?;
    validate_recursive_rpc_optional_nonempty("idempotency_key", params.idempotency_key.as_deref())?;
    validate_recursive_rpc_optional_nonempty("model", params.model.as_deref())?;
    validate_recursive_rpc_optional_nonempty("effort", params.effort.as_deref())?;
    validate_recursive_rpc_optional_nonempty("approval_mode", params.approval_mode.as_deref())?;
    if params.heartbeat_ttl_ms.is_some() {
        return Err(DaemonError::InvalidParam(
            "Invalid params: heartbeat_ttl_ms is not supported because the recursive DAG live scheduler heartbeat loop is disabled".to_string(),
        ));
    }
    if params.output_repair_attempts.unwrap_or(0) != 0 {
        return Err(DaemonError::InvalidParam(
            "Invalid params: output_repair_attempts must be 0 because live output repair is not wired into the request-scoped scheduler".to_string(),
        ));
    }
    if let Some(policy) = &params.tool_policy {
        for tool in policy
            .allowed_tools
            .iter()
            .chain(policy.denied_tools.iter())
        {
            validate_recursive_rpc_nonempty("tool_policy tool", tool)?;
        }
    }
    if let Some(policy) = &params.sandbox_policy {
        validate_recursive_rpc_optional_nonempty(
            "sandbox_policy.requested_branch",
            policy.requested_branch.as_deref(),
        )?;
    }
    Ok(())
}

pub(super) fn recursive_live_scheduler_request_fingerprint(
    params: &RunRecursiveLiveSchedulerParams,
) -> Result<String> {
    let value = serde_json::to_value(params)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(value.to_string().as_bytes())
    ))
}

pub(super) fn recursive_live_scheduler_policy_snapshot(
    params: &RunRecursiveLiveSchedulerParams,
) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "gate": "recursive_dag_live_scheduler_control_enabled",
        "reachability": "explicit_manual_rpc",
        "execution_path": "ordinary_recursive_dag_live_scheduler",
        "topology_live_scheduler": false,
        "background_loop_enabled": false,
        "heartbeat_loop_enabled": false,
        "output_repair_attempts": params.output_repair_attempts.unwrap_or(0),
        "request": serde_json::to_value(params)?,
    }))
}

pub(super) fn recursive_live_tool_policy(
    policy: Option<&rsi_common::RecursiveLiveToolPolicy>,
) -> RecursiveDagLiveToolPolicy {
    RecursiveDagLiveToolPolicy {
        allowed_tools: policy
            .map(|policy| policy.allowed_tools.clone())
            .unwrap_or_default(),
        denied_tools: policy
            .map(|policy| policy.denied_tools.clone())
            .unwrap_or_default(),
    }
}

pub(super) fn recursive_live_sandbox_policy(
    policy: Option<&rsi_common::RecursiveLiveSandboxPolicy>,
    sandbox: Option<&rsi_common::types::SandboxSpec>,
) -> RecursiveDagLiveSandboxPolicy {
    RecursiveDagLiveSandboxPolicy {
        requested_kind: policy
            .and_then(|policy| policy.requested_kind)
            .or_else(|| sandbox.and_then(|sandbox| sandbox.kind)),
        requested_branch: policy
            .and_then(|policy| policy.requested_branch.clone())
            .or_else(|| sandbox.and_then(|sandbox| sandbox.branch.clone())),
        preserve_on_failure: policy.and_then(|policy| policy.preserve_on_failure),
        allowed_write_roots: policy
            .map(|policy| policy.allowed_write_roots.clone())
            .unwrap_or_default(),
    }
}

impl RpcServer {
    pub(super) async fn handle_continue_recursive_recovery(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_recovery_control_enabled()?;
        let params: ContinueRecursiveRecoveryParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        if params.max_graphs == 0 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: max_graphs must be positive".to_string(),
            ));
        }
        if params
            .time_budget_ms
            .is_some_and(|time_budget_ms| time_budget_ms > i64::MAX as u64)
        {
            return Err(DaemonError::InvalidParam(
                "Invalid params: time_budget_ms is out of range".to_string(),
            ));
        }
        let budget = RecursiveRecoveryBudget {
            max_graphs: params.max_graphs,
            time_budget_ms: params.time_budget_ms,
            source: RecursiveRecoverySource::ManualRpc,
        };
        let store = self.session_manager.store().clone();
        let pass = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.continue_deferred_recursive_recovery(budget)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(pass)?)
    }

    pub(super) async fn handle_continue_topology_recursive_recovery(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_recovery_control_enabled()?;
        let mut params: ContinueTopologyRecursiveRecoveryParams = if request.params.is_null() {
            ContinueTopologyRecursiveRecoveryParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        if params.max_graphs == 0 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: max_graphs must be positive".to_string(),
            ));
        }
        if params
            .time_budget_ms
            .is_some_and(|time_budget_ms| time_budget_ms > i64::MAX as u64)
        {
            return Err(DaemonError::InvalidParam(
                "Invalid params: time_budget_ms is out of range".to_string(),
            ));
        }
        validate_recursive_rpc_optional_nonempty("node_id", params.node_id.as_deref())?;
        params.node_id = params.node_id.map(|node_id| node_id.trim().to_string());
        validate_recursive_rpc_optional_nonempty(
            "execution_owner",
            params.execution_owner.as_deref(),
        )?;
        params.execution_owner = params
            .execution_owner
            .map(|execution_owner| execution_owner.trim().to_string());
        validate_recursive_rpc_optional_nonempty(
            "idempotency_key",
            params.idempotency_key.as_deref(),
        )?;
        params.idempotency_key = params
            .idempotency_key
            .map(|idempotency_key| idempotency_key.trim().to_string());

        let execution_owner = params
            .execution_owner
            .clone()
            .unwrap_or_else(|| RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string());
        if execution_owner != RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE {
            return Err(DaemonError::InvalidParam(format!(
                "Invalid params: recursive topology recovery supports execution_owner={} only; requested {}",
                RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE, execution_owner
            )));
        }
        if params.apply_to == rsi_common::TopologyRecursiveRecoveryApplyTo::AllMatching {
            if params.graph_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: apply_to=all_matching must not include graph_id".to_string(),
                ));
            }
            if params.topology_id.is_none() && params.workflow_execution_id.is_none() {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: apply_to=all_matching requires topology_id or workflow_execution_id".to_string(),
                ));
            }
        } else {
            let has_mutation_selector = params.graph_id.is_some()
                || params.topology_id.is_some()
                || params.workflow_execution_id.is_some()
                || params.node_id.is_some();
            if !has_mutation_selector {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: recursive topology recovery requires graph_id, topology_id, workflow_execution_id, or node_id".to_string(),
                ));
            }
        }

        let store_request = crate::store::recursive_dag::TopologyRecursiveRecoveryRequest {
            graph_id: params.graph_id.map(RecursiveTaskGraphId),
            topology_id: params.topology_id,
            project_id: params.project_id,
            workflow_id: params.workflow_id,
            workflow_execution_id: params.workflow_execution_id,
            node_id: params.node_id,
            topology_iteration: params.topology_iteration,
            parent_session_id: params.parent_session_id,
            execution_owner,
            apply_to: params.apply_to,
            max_graphs: params.max_graphs,
            time_budget_ms: params.time_budget_ms,
            idempotency_key: params.idempotency_key,
        };
        let store = self.session_manager.store().clone();
        let response = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .continue_topology_recursive_recovery(store_request)
                .map_err(topology_recovery_rpc_error)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_request_recursive_graph_cancellation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_cancellation_control_enabled()?;
        let params: RequestRecursiveGraphCancellationParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        validate_recursive_rpc_nonempty("reason", &params.reason)?;
        if let Some(requested_by) = params.requested_by.as_deref() {
            validate_recursive_rpc_nonempty("requested_by", requested_by)?;
        }
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let create = crate::store::recursive_dag::RecursiveCancellationRequestCreate {
            reason: params.reason,
            requested_by: params.requested_by,
            source: RecursiveCancellationRequestSource::ManualRpc,
        };
        let store = self.session_manager.store().clone();
        let cancellation = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            store.request_recursive_graph_cancellation(graph_id, create)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(cancellation)?)
    }

    pub(super) async fn handle_request_recursive_scheduler_run_cancellation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_cancellation_control_enabled()?;
        let params: RequestRecursiveSchedulerRunCancellationParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        validate_recursive_rpc_nonempty("reason", &params.reason)?;
        if let Some(requested_by) = params.requested_by.as_deref() {
            validate_recursive_rpc_nonempty("requested_by", requested_by)?;
        }
        let run_id = RecursiveSchedulerRunId(params.run_id);
        let create = crate::store::recursive_dag::RecursiveCancellationRequestCreate {
            reason: params.reason,
            requested_by: params.requested_by,
            source: RecursiveCancellationRequestSource::ManualRpc,
        };
        let store = self.session_manager.store().clone();
        let cancellation = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_recursive_scheduler_run(run_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive scheduler run not found: {run_id}"))
            })?;
            store.request_recursive_scheduler_run_cancellation(run_id, create)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(cancellation)?)
    }

    pub(super) async fn handle_request_topology_recursive_cancellation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_cancellation_control_enabled()?;
        let mut params: RequestTopologyRecursiveCancellationParams = if request.params.is_null() {
            RequestTopologyRecursiveCancellationParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        validate_recursive_rpc_nonempty("reason", &params.reason)?;
        params.reason = params.reason.trim().to_string();
        if let Some(requested_by) = params.requested_by.as_deref() {
            validate_recursive_rpc_nonempty("requested_by", requested_by)?;
        }
        params.requested_by = params
            .requested_by
            .map(|requested_by| requested_by.trim().to_string());
        validate_recursive_rpc_optional_nonempty("node_id", params.node_id.as_deref())?;
        params.node_id = params.node_id.map(|node_id| node_id.trim().to_string());
        validate_recursive_rpc_optional_nonempty(
            "execution_owner",
            params.execution_owner.as_deref(),
        )?;
        params.execution_owner = params
            .execution_owner
            .map(|execution_owner| execution_owner.trim().to_string());
        if let Some(idempotency_key) = params.idempotency_key.as_deref() {
            validate_recursive_rpc_nonempty("idempotency_key", idempotency_key)?;
        }
        params.idempotency_key = params
            .idempotency_key
            .map(|idempotency_key| idempotency_key.trim().to_string());

        let execution_owner = params
            .execution_owner
            .clone()
            .unwrap_or_else(|| RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string());
        if execution_owner != RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE {
            return Err(DaemonError::InvalidParam(format!(
                "recursive topology cancellation supports execution_owner={} only; requested {}",
                RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE, execution_owner
            )));
        }
        if params.scope == rsi_common::TopologyRecursiveCancellationScope::Graph {
            if params.run_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: graph-scoped recursive topology cancellation must not include run_id"
                        .to_string(),
                ));
            }
            if params.run_selection != rsi_common::TopologyRecursiveCancellationRunSelection::Active
            {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: graph-scoped recursive topology cancellation must not set run_selection"
                        .to_string(),
                ));
            }
        }
        if params.scope == rsi_common::TopologyRecursiveCancellationScope::Run
            && params.apply_to == rsi_common::TopologyRecursiveCancellationApplyTo::AllMatching
        {
            return Err(DaemonError::InvalidParam(
                "Invalid params: run-scoped recursive topology cancellation does not support apply_to=all_matching"
                    .to_string(),
            ));
        }
        if params.apply_to == rsi_common::TopologyRecursiveCancellationApplyTo::AllMatching {
            if params.topology_id.is_none() {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: apply_to=all_matching requires topology_id".to_string(),
                ));
            }
            if params.graph_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "Invalid params: apply_to=all_matching must not include graph_id".to_string(),
                ));
            }
        }

        let store_request = crate::store::recursive_dag::TopologyRecursiveCancellationRequest {
            graph_id: params.graph_id.map(RecursiveTaskGraphId),
            topology_id: params.topology_id,
            project_id: params.project_id,
            workflow_id: params.workflow_id,
            workflow_execution_id: params.workflow_execution_id,
            node_id: params.node_id,
            topology_iteration: params.topology_iteration,
            parent_session_id: params.parent_session_id,
            execution_owner,
            scope: params.scope,
            apply_to: params.apply_to,
            run_id: params.run_id.map(RecursiveSchedulerRunId),
            run_selection: params.run_selection,
            reason: params.reason,
            requested_by: params.requested_by,
            idempotency_key: params.idempotency_key,
        };
        let store = self.session_manager.store().clone();
        let response = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .request_topology_recursive_cancellation(store_request)
                .map_err(|error| DaemonError::InvalidParam(error.to_string()))
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_run_recursive_fake_scheduler(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_scheduler_control_enabled()?;
        let params: RunRecursiveFakeSchedulerParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        if params.max_steps == 0 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: max_steps must be positive".to_string(),
            ));
        }
        if params.max_steps > MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS {
            return Err(DaemonError::InvalidParam(format!(
                "Invalid params: max_steps must be <= {MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS}"
            )));
        }
        if let Some(mode) = params.execution_mode.as_deref() {
            let normalized = mode.trim().to_ascii_lowercase();
            if normalized != "fake" {
                return Err(DaemonError::InvalidParam(format!(
                    "recursive DAG scheduler RPC is fake-only in Phase 5A.5; requested execution_mode={mode}"
                )));
            }
        }
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        if let Some(operator) = params.operator.as_deref() {
            validate_recursive_rpc_nonempty("operator", operator)?;
        }
        let operator = Some(params.operator.unwrap_or_else(|| "rpc".to_string()));
        let lease_ttl_ms = self
            .runtime_config
            .recursive_dag_run_lease_ttl_ms
            .load(Ordering::Relaxed);
        let lease_ttl_seconds =
            i64::try_from(lease_ttl_ms.saturating_add(999) / 1000).map_err(|_| {
                DaemonError::InvalidParam(
                    "recursive_dag_run_lease_ttl_ms exceeds i64 seconds range".to_string(),
                )
            })?;
        let max_active_runs = self
            .runtime_config
            .recursive_dag_max_concurrent_graphs
            .load(Ordering::Relaxed);
        let store = self.session_manager.store().clone();
        let run = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            let lease_policy = crate::store::recursive_dag::RecursiveSchedulerLeasePolicy {
                lease_owner: "rsid-recursive-dag-manual-rpc".to_string(),
                lease_ttl_seconds,
                max_active_runs,
            };
            let mut scheduler = crate::recursive_dag::RecursiveDagScheduler::new(
                crate::recursive_dag::RecursiveDagFakeExecutor::new(),
            );
            let report = scheduler.run_until_idle_with_options(
                &store,
                graph_id,
                params.max_steps,
                rsi_common::RecursiveSchedulerRunSource::ManualRpc,
                operator,
                lease_policy,
            )?;
            store
                .load_recursive_scheduler_run(report.run_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive scheduler run missing after manual RPC run: {}",
                        report.run_id
                    ))
                })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(run)?)
    }

    pub(super) async fn handle_run_recursive_live_scheduler(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_live_scheduler_control_enabled()?;
        let params: RunRecursiveLiveSchedulerParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        validate_recursive_live_scheduler_params(&params)?;

        let graph_id = RecursiveTaskGraphId(params.graph_id);
        self.ensure_recursive_live_scheduler_graph_is_ordinary(graph_id)
            .await?;
        let operator = Some(params.operator.clone().unwrap_or_else(|| "rpc".to_string()));
        let request_fingerprint = recursive_live_scheduler_request_fingerprint(&params)?;
        let lease_ttl_ms = self
            .runtime_config
            .recursive_dag_run_lease_ttl_ms
            .load(Ordering::Relaxed);
        let lease_ttl_seconds =
            i64::try_from(lease_ttl_ms.saturating_add(999) / 1000).map_err(|_| {
                DaemonError::InvalidParam(
                    "recursive_dag_run_lease_ttl_ms exceeds i64 seconds range".to_string(),
                )
            })?;
        let max_active_runs = self
            .runtime_config
            .recursive_dag_max_concurrent_graphs
            .load(Ordering::Relaxed);

        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&self.session_manager));
        let mut driver = binding.enabled_driver();
        let report = driver
            .run_request_scoped(RecursiveDagLiveSchedulerRunRequest {
                graph_id,
                max_steps: params.max_steps,
                source: RecursiveSchedulerRunSource::ManualRpc,
                operator,
                idempotency_key: params.idempotency_key.clone(),
                request_fingerprint: Some(request_fingerprint),
                policy_snapshot: recursive_live_scheduler_policy_snapshot(&params)?,
                provider: params.provider,
                model: params.model.clone(),
                effort: params.effort.clone(),
                working_dir: params.working_dir.clone(),
                sandbox: params.sandbox.clone(),
                sandbox_worktree_id: None,
                workflow_execution_id: None,
                topology_workflow_id: None,
                max_wall_time_ms: params.max_wall_time_ms,
                budgets: RecursiveDagLiveBudgetPlaceholders {
                    max_wall_time_ms: params.max_wall_time_ms,
                    token_budget: params.token_budget,
                    tool_call_budget: params.tool_call_budget,
                    artifact_bytes: params.artifact_bytes_budget,
                },
                approval_policy: RecursiveDagLiveApprovalPolicy {
                    policy_name: params.approval_mode.clone(),
                    require_operator_approval: None,
                },
                tool_policy: recursive_live_tool_policy(params.tool_policy.as_ref()),
                sandbox_policy: recursive_live_sandbox_policy(
                    params.sandbox_policy.as_ref(),
                    params.sandbox.as_ref(),
                ),
                lease_policy: crate::store::recursive_dag::RecursiveSchedulerLeasePolicy {
                    lease_owner: "rsid-recursive-dag-live-manual-rpc".to_string(),
                    lease_ttl_seconds,
                    max_active_runs,
                },
            })
            .await?;

        let store = self.session_manager.store().clone();
        let response = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let scheduler_run = store
                .load_recursive_scheduler_run(report.scheduler_run.id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive live scheduler run missing after manual RPC run: {}",
                        report.scheduler_run.id
                    ))
                })?;
            let graph = store
                .get_recursive_task_graph(scheduler_run.graph_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive DAG graph not found after live scheduler run: {}",
                        scheduler_run.graph_id
                    ))
                })?;
            let live_attempt_details =
                store.list_recursive_live_attempts_for_scheduler_run(scheduler_run.id)?;
            let live_attempt_ids = live_attempt_details
                .iter()
                .map(|live| live.summary.id)
                .collect::<Vec<_>>();
            let latest_validations = store
                .latest_recursive_live_output_validation_summaries_for_live_attempts(
                    &live_attempt_ids,
                )?;
            let validation_summaries = latest_validations.values().cloned().collect();
            let mut live_attempts = Vec::with_capacity(live_attempt_details.len());
            for live in live_attempt_details {
                let task = graph
                    .nodes
                    .iter()
                    .find(|task| task.id == live.summary.task_id)
                    .cloned()
                    .ok_or_else(|| {
                        DaemonError::Store(format!(
                            "recursive task not found for live attempt {}: {}",
                            live.summary.id, live.summary.task_id
                        ))
                    })?;
                let recursive_attempt = graph
                    .attempts
                    .iter()
                    .find(|attempt| attempt.id == live.summary.attempt_id)
                    .cloned()
                    .ok_or_else(|| {
                        DaemonError::Store(format!(
                            "recursive attempt not found for live attempt {}: {}",
                            live.summary.id, live.summary.attempt_id
                        ))
                    })?;
                let session = live
                    .summary
                    .session_id
                    .map(|session_id| store.load_recursive_live_linked_session_summary(session_id))
                    .transpose()?
                    .flatten();
                let heartbeat = store
                    .load_recursive_live_attempt_heartbeat(live.summary.id)?
                    .map(redact_recursive_live_heartbeat_token);
                let latest_interrupt =
                    store.load_existing_recursive_live_interrupt_for_attempt(live.summary.id)?;
                live_attempts.push(rsi_common::RecursiveLiveAttemptReadback {
                    live_attempt: live.clone(),
                    task,
                    recursive_attempt,
                    scheduler_run: scheduler_run.clone(),
                    session,
                    heartbeat,
                    latest_interrupt,
                    latest_validation: latest_validations.get(&live.summary.id).cloned(),
                    artifacts: None,
                    retry_history: None,
                    warnings: Vec::new(),
                });
            }
            let report_artifact = scheduler_run.report_artifact_id.and_then(|artifact_id| {
                graph
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.id == artifact_id)
                    .cloned()
            });
            let mut warnings = Vec::new();
            if let Some(failure_reason) = report.failure_reason.clone() {
                warnings.push(RecursiveReadbackWarning {
                    code: "live_scheduler_launch_boundary".to_string(),
                    message: failure_reason,
                    resource_type: Some("recursive_scheduler_run".to_string()),
                    resource_id: Some(scheduler_run.id.to_string()),
                });
            }
            warnings.push(RecursiveReadbackWarning {
                code: "live_scheduler_background_loop_disabled".to_string(),
                message: "explicit live scheduler RPC is request-scoped; background scheduling remains disabled".to_string(),
                resource_type: Some("recursive_scheduler_run".to_string()),
                resource_id: Some(scheduler_run.id.to_string()),
            });
            Ok::<_, DaemonError>(RunRecursiveLiveSchedulerResponse {
                stop_reason: scheduler_run
                    .stop_reason
                    .unwrap_or(report.stop_reason),
                step_count: scheduler_run.step_count,
                selected_task_order: report.selected_task_order,
                live_attempts,
                validation_summaries,
                report_artifact,
                cancellation_request_id: scheduler_run.cancellation_request_id,
                warnings,
                scheduler_run,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_commit_recursive_live_attempt_output(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_live_scheduler_control_enabled()?;
        let params: CommitRecursiveLiveAttemptOutputParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;

        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&self.session_manager));
        let committed = binding
            .commit_completed_live_attempt_output(params.live_attempt_id)
            .await?;
        let commit_result = match committed {
            RecursiveDagLiveOutputCommitResult::Committed { result } => result,
            RecursiveDagLiveOutputCommitResult::NotReady {
                live_attempt,
                reason,
            } => {
                return Err(DaemonError::InvalidParam(
                    recursive_live_output_not_ready_message(live_attempt.summary.id, reason),
                ));
            }
        };

        let store = self.session_manager.store().clone();
        let response = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let scheduler_run = store
                .load_recursive_scheduler_run(commit_result.live_attempt.summary.scheduler_run_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive scheduler run not found for live attempt {}: {}",
                        commit_result.live_attempt.summary.id,
                        commit_result.live_attempt.summary.scheduler_run_id
                    ))
                })?;
            let session = commit_result
                .live_attempt
                .summary
                .session_id
                .map(|session_id| store.load_recursive_live_linked_session_summary(session_id))
                .transpose()?
                .flatten();
            let heartbeat = store
                .load_recursive_live_attempt_heartbeat(commit_result.live_attempt.summary.id)?
                .map(redact_recursive_live_heartbeat_token);
            let latest_interrupt = store.load_existing_recursive_live_interrupt_for_attempt(
                commit_result.live_attempt.summary.id,
            )?;
            let graph = store
                .get_recursive_task_graph(commit_result.live_attempt.summary.graph_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive DAG graph not found after live output commit: {}",
                        commit_result.live_attempt.summary.graph_id
                    ))
                })?;
            let retry_history = Some(
                graph
                    .attempts
                    .iter()
                    .filter(|attempt| attempt.task_id == commit_result.live_attempt.summary.task_id)
                    .cloned()
                    .collect(),
            );
            let latest_validation = Some(commit_result.validation_result.summary.clone());
            let response = CommitRecursiveLiveAttemptOutputResponse {
                readback: rsi_common::RecursiveLiveAttemptReadback {
                    live_attempt: commit_result.live_attempt,
                    task: commit_result.task,
                    recursive_attempt: commit_result.recursive_attempt,
                    scheduler_run,
                    session,
                    heartbeat,
                    latest_interrupt,
                    latest_validation,
                    artifacts: None,
                    retry_history,
                    warnings: Vec::new(),
                },
                validation_result: commit_result.validation_result,
                raw_output_artifact: commit_result.raw_output_artifact,
                normalized_output_artifact: commit_result.normalized_output_artifact,
                validation_artifact: commit_result.validation_artifact,
            };
            Ok::<_, DaemonError>(response)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_run_recursive_topology_node_fake_scheduler(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_recursive_scheduler_control_enabled()?;
        let params: RunRecursiveTopologyNodeFakeSchedulerParams = if request.params.is_null() {
            RunRecursiveTopologyNodeFakeSchedulerParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        validate_recursive_rpc_optional_nonempty("node_id", params.node_id.as_deref())?;
        if let Some(operator) = params.operator.as_deref() {
            validate_recursive_rpc_nonempty("operator", operator)?;
        }
        let max_steps = params.max_steps.ok_or_else(|| {
            DaemonError::InvalidParam("Invalid params: max_steps is required".to_string())
        })?;
        if max_steps == 0 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: max_steps must be positive".to_string(),
            ));
        }
        if max_steps > MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS {
            return Err(DaemonError::InvalidParam(format!(
                "Invalid params: max_steps must be <= {MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS}"
            )));
        }
        if let Some(mode) = params.execution_mode.as_deref() {
            let normalized = mode.trim().to_ascii_lowercase();
            if normalized != "fake" {
                return Err(DaemonError::InvalidParam(format!(
                    "recursive topology scheduler RPC is fake-only; requested execution_mode={mode}"
                )));
            }
        }

        let operator = Some(params.operator.unwrap_or_else(|| "rpc".to_string()));
        let lease_ttl_ms = self
            .runtime_config
            .recursive_dag_run_lease_ttl_ms
            .load(Ordering::Relaxed);
        let lease_ttl_seconds =
            i64::try_from(lease_ttl_ms.saturating_add(999) / 1000).map_err(|_| {
                DaemonError::InvalidParam(
                    "recursive_dag_run_lease_ttl_ms exceeds i64 seconds range".to_string(),
                )
            })?;
        let max_active_runs = self
            .runtime_config
            .recursive_dag_max_concurrent_graphs
            .load(Ordering::Relaxed);
        let graph_filter = crate::store::recursive_dag::RecursiveTopologyGraphListFilter {
            graph_id: params.graph_id.map(RecursiveTaskGraphId),
            topology_id: params.topology_id,
            project_id: None,
            workflow_id: None,
            workflow_execution_id: params.workflow_execution_id,
            node_id: params.node_id,
            topology_iteration: params.topology_iteration,
            parent_session_id: None,
            execution_owner: Some(RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string()),
            include_quarantined: true,
        };
        let store = self.session_manager.store().clone();
        let response = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let context = store
                .resolve_recursive_topology_fake_scheduler_graph(graph_filter)
                .map_err(|error| DaemonError::InvalidParam(error.to_string()))?;
            let lease_policy = crate::store::recursive_dag::RecursiveSchedulerLeasePolicy {
                lease_owner: "rsid-recursive-topology-manual-rpc".to_string(),
                lease_ttl_seconds,
                max_active_runs,
            };
            let mut scheduler = crate::recursive_dag::RecursiveDagScheduler::new(
                crate::recursive_dag::RecursiveDagFakeExecutor::new(),
            );
            let report = scheduler.run_until_idle_with_options(
                &store,
                context.graph.id,
                max_steps,
                rsi_common::RecursiveSchedulerRunSource::ManualRpc,
                operator,
                lease_policy,
            )?;
            let scheduler_run = store
                .load_recursive_scheduler_run(report.run_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive scheduler run missing after topology fake scheduler run: {}",
                        report.run_id
                    ))
                })?;
            let graph_detail = store
                .get_recursive_task_graph(context.graph.id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive DAG graph missing after topology fake scheduler run: {}",
                        context.graph.id
                    ))
                })?;
            let report_artifact = scheduler_run.report_artifact_id.and_then(|artifact_id| {
                graph_detail
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.id == artifact_id)
                    .cloned()
            });
            let status = store.get_topology_recursive_status(
                crate::store::recursive_dag::TopologyRecursiveStatusFilter {
                    graph_id: Some(context.graph.id),
                    topology_id: Some(context.graph_link.topology_id),
                    project_id: context.graph_link.project_id.or(context.graph.project_id),
                    workflow_id: context.graph_link.workflow_id.or(context.graph.workflow_id),
                    workflow_execution_id: context.graph_link.workflow_execution_id,
                    node_id: Some(context.graph_link.source_topology_node_id.clone()),
                    topology_iteration: Some(context.graph_link.source_topology_iteration),
                    parent_session_id: context.graph_link.parent_session_id,
                    execution_owner: Some(RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string()),
                    include_dynamic_children: true,
                },
            )?;
            Ok::<_, DaemonError>(RunRecursiveTopologyNodeFakeSchedulerResponse {
                scheduler_run,
                report_artifact,
                graph_link: context.graph_link,
                task_links: context.task_links,
                status,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(response)?)
    }

    pub(super) fn ensure_recursive_recovery_control_enabled(&self) -> Result<()> {
        if self
            .runtime_config
            .recursive_dag_recovery_controls_enabled
            .load(Ordering::Relaxed)
        {
            Ok(())
        } else {
            Err(DaemonError::InvalidParam(
                "recursive DAG recovery control RPCs are disabled; set recursive_dag_recovery_controls_enabled=true".to_string(),
            ))
        }
    }

    pub(super) fn ensure_recursive_scheduler_control_enabled(&self) -> Result<()> {
        if self
            .runtime_config
            .recursive_dag_scheduler_controls_enabled
            .load(Ordering::Relaxed)
        {
            Ok(())
        } else {
            Err(DaemonError::InvalidParam(
                "recursive DAG fake scheduler control RPCs are disabled; set recursive_dag_scheduler_controls_enabled=true".to_string(),
            ))
        }
    }

    pub(super) fn ensure_recursive_live_scheduler_control_enabled(&self) -> Result<()> {
        if self
            .runtime_config
            .recursive_dag_live_scheduler_control_enabled
            .load(Ordering::Relaxed)
        {
            Ok(())
        } else {
            Err(DaemonError::InvalidParam(
                "recursive DAG live scheduler control RPC is disabled; set recursive_dag_live_scheduler_control_enabled=true".to_string(),
            ))
        }
    }

    pub(super) fn ensure_gv_render_recursive_origin_enabled(&self) -> Result<()> {
        if self
            .runtime_config
            .gv_render_recursive_origin
            .load(Ordering::Relaxed)
        {
            Ok(())
        } else {
            Err(DaemonError::InvalidParam(
                "gv recursive-origin rendering is disabled; set gv_render_recursive_origin=true"
                    .to_string(),
            ))
        }
    }

    pub(super) async fn ensure_recursive_live_scheduler_graph_is_ordinary(
        &self,
        graph_id: RecursiveTaskGraphId,
    ) -> Result<()> {
        let store = self.session_manager.store().clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let graph = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            if graph.graph.topology_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "recursive DAG live scheduler is ordinary-graph only; topology-linked graphs remain disabled until topology live delegation is explicitly implemented"
                        .to_string(),
                ));
            }
            Ok(())
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))?
    }

    pub(super) fn ensure_recursive_cancellation_control_enabled(&self) -> Result<()> {
        if self
            .runtime_config
            .recursive_dag_cancellation_controls_enabled
            .load(Ordering::Relaxed)
        {
            Ok(())
        } else {
            Err(DaemonError::InvalidParam(
                "recursive DAG cancellation control RPCs are disabled; set recursive_dag_cancellation_controls_enabled=true".to_string(),
            ))
        }
    }
}
