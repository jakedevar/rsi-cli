use super::*;

/// RPC params for listing workflows with optional project filter.
#[derive(Debug, Default, Deserialize)]
pub struct ListWorkflowsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// RPC params for getting a single workflow by ID.
#[derive(Debug, Deserialize)]
pub struct GetWorkflowParams {
    pub workflow_id: Uuid,
}

impl RpcServer {
    /// ListTopologies — return all topologies (optionally name-prefix filtered).
    pub(super) async fn handle_list_topologies(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListTopologiesParams = serde_json::from_value(request.params.clone())
            .unwrap_or(ListTopologiesParams { name_prefix: None });

        let topologies = self
            .session_manager
            .list_topologies(params.name_prefix)
            .await?;
        Ok(serde_json::to_value(&topologies)?)
    }

    /// CreateTopology — daemon validates DAG, kinds, iteration caps, name
    /// uniqueness, then inserts. Returns `{ id: Uuid }`.
    pub(super) async fn handle_create_topology(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateTopologyParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let id = self
            .session_manager
            .create_topology(params.name, params.definition)
            .await?;
        Ok(serde_json::json!({ "id": id }))
    }

    /// UpdateTopology — daemon validates (when definition is Some) and
    /// rejects rename to an existing name. Either field may be None.
    ///
    /// P1.12 §9: after a successful update, re-bridge and upsert the
    /// corresponding workflow row so edits flow through to the `gv` picker.
    pub(super) async fn handle_update_topology(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateTopologyParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let topology_id = params.id;
        // #633: `shared` may be sent alone; with no field at all the
        // existing "nothing to update" refusal still applies.
        if params.name.is_some() || params.definition.is_some() || params.shared.is_none() {
            self.session_manager
                .update_topology(params.id, params.name, params.definition)
                .await?;
        }
        if let Some(shared) = params.shared {
            self.session_manager
                .set_topology_shared(topology_id, shared)
                .await?;
        }

        // Re-bridge into workflows table for gv-picker freshness. Best-effort:
        // a stale workflow row is recoverable on the next ExecuteTopology;
        // updates are not gated on the upsert succeeding. Log on failure.
        match self.session_manager.get_topology(topology_id).await {
            Ok(updated) => {
                if let Err(e) = self
                    .session_manager
                    .upsert_bridged_workflow(&updated, None)
                    .await
                {
                    tracing::warn!("UpdateTopology: bridge upsert failed for {topology_id}: {e:#}");
                }
            }
            Err(e) => {
                tracing::warn!(
                    "UpdateTopology: get_topology after update failed for {topology_id}: {e:#}"
                );
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    /// DeleteTopology — daemon rejects if any Epic still references the
    /// topology via workflow_id. Override pointers are not checked
    /// (Decision 1 in plan).
    ///
    /// P1.12 §9: cascade-delete the mirrored workflow row FIRST, then the
    /// topology row. If the workflow delete fails, the topology delete is
    /// skipped and the error surfaces; if the topology delete fails after the
    /// workflow delete succeeds, next `ExecuteTopology` recreates the row.
    pub(super) async fn handle_delete_topology(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: DeleteTopologyParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        // OQ2 ordering: workflow row first, topology row second.
        self.session_manager
            .delete_workflow_by_source_topology(params.id)
            .await?;
        self.session_manager.delete_topology(params.id).await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// GetTopology — fetch a single topology by id.
    pub(super) async fn handle_get_topology(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetTopologyParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let topology = self.session_manager.get_topology(params.id).await?;
        Ok(serde_json::to_value(&topology)?)
    }

    pub(super) async fn handle_list_workflows(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListWorkflowsParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();
        let workflows = self
            .session_manager
            .list_workflows(params.project_id)
            .await?;
        Ok(serde_json::to_value(&workflows)?)
    }

    pub(super) async fn handle_get_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetWorkflowParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let workflow = self
            .session_manager
            .get_workflow(params.workflow_id)
            .await?;
        Ok(serde_json::to_value(&workflow)?)
    }

    pub(super) async fn handle_get_workflow_definition(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::GetWorkflowDefinitionParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let document = self
            .session_manager
            .get_workflow_definition(params.workflow_id)
            .await?
            .ok_or_else(|| {
                DaemonError::Store(format!("Workflow not found: {}", params.workflow_id))
            })?;
        let response = rsi_common::rpc::GetWorkflowDefinitionResponse { document };
        Ok(serde_json::to_value(&response)?)
    }

    pub(super) async fn handle_upsert_workflow_definition(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::UpsertWorkflowDefinitionParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let document = self
            .session_manager
            .upsert_workflow_definition(params.document)
            .await?;
        let response = rsi_common::rpc::UpsertWorkflowDefinitionResponse { document };
        Ok(serde_json::to_value(&response)?)
    }

    pub(super) async fn handle_generate_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::GenerateWorkflowParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let skip_cache = params.use_cache == Some(false);

        // Check in-memory cache first (unless explicitly disabled).
        // content_hash keys on the RESOLVED intent content (D6 content-staleness);
        // at this GenerateWorkflow entry point the resolved input content is the
        // intent itself, so it doubles as the content source.
        let cache_key = rsi_graph::cache::CacheKey::new(
            &params.intent,
            &params.intent,
            params.model.as_deref(),
            rsi_graph::cache::TOPOLOGY_VERSION,
        );

        if !skip_cache {
            let mut cache = self.session_manager.graph_cache().lock().await;
            if let Some(entry) = cache.get(&cache_key) {
                let response = rsi_common::rpc::GenerateWorkflowResponse {
                    workflow: serde_json::to_value(&entry.workflow)?,
                    reasoning: entry.reasoning.clone(),
                    cache_hit: true,
                };
                return Ok(serde_json::to_value(&response)?);
            }
        }

        // Cache miss — generate via topology/freeform routing.
        let result = rsi_graph::generate::generate_workflow(&params.intent)
            .map_err(|e| DaemonError::Rpc(format!("Generation failed: {}", e)))?;

        // Insert into in-memory cache.
        let entry = rsi_graph::cache::CacheEntry {
            key: cache_key,
            workflow: result.workflow.clone(),
            reasoning: result.reasoning.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            hit_count: 0,
        };
        self.session_manager
            .graph_cache()
            .lock()
            .await
            .insert(entry);

        let response = rsi_common::rpc::GenerateWorkflowResponse {
            workflow: serde_json::to_value(&result.workflow)?,
            reasoning: result.reasoning,
            cache_hit: false,
        };

        Ok(serde_json::to_value(&response)?)
    }

    pub(super) async fn handle_refine_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::RefineWorkflowParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let workflow: rsi_graph::format::WorkflowDefinition =
            serde_json::from_value(params.workflow)
                .map_err(|e| DaemonError::Rpc(format!("Invalid workflow: {}", e)))?;

        let result = rsi_graph::generate::refine::refine_workflow(&workflow, &params.instruction)
            .map_err(|e| DaemonError::Rpc(format!("Refinement failed: {}", e)))?;

        let response = rsi_common::rpc::RefineWorkflowResponse {
            workflow: serde_json::to_value(&result.workflow)?,
            changes_made: result.changes_made,
            valid: result.valid,
        };

        Ok(serde_json::to_value(&response)?)
    }

    pub(super) async fn handle_execute_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::ExecuteWorkflowParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        // P1.12: validate parent_id (if provided) before any executor work.
        self.validate_parent_id(params.parent_id).await?;

        let workflow: rsi_graph::format::WorkflowDefinition =
            serde_json::from_value(params.workflow)
                .map_err(|e| DaemonError::Rpc(format!("Invalid workflow: {}", e)))?;

        let working_dir = params.working_dir.map(std::path::PathBuf::from);

        let response = SessionManager::execute_workflow_live(
            Arc::clone(&self.session_manager),
            params.workflow_id,
            workflow.clone(),
            params.input,
            params.dry_run,
            params.project_id,
            working_dir,
            params.parent_id,
        )
        .await?;

        // Auto-promote-to-chain: if the workflow is "Master Improve", register a chain
        // iteration row so the chain_driver picks up the eventual Succeeded event and
        // respawns. This means the picker route (non-RPC) gets loop semantics for free.
        if workflow.name == "Master Improve" {
            let store = self.session_manager.store().clone();
            let execution_id = response.execution_id;
            let initial_goal = workflow
                .nodes
                .iter()
                .find(|n| n.id == "entry")
                .map(|n| n.instructions.clone())
                .unwrap_or_default();
            tokio::spawn(async move {
                let chain_id = Uuid::new_v4();
                let iter = rsi_common::types::ChainIteration {
                    chain_id,
                    iteration_index: 0,
                    parent_execution_id: None,
                    child_execution_id: execution_id,
                    halt_reason: None,
                    goal_text: initial_goal,
                    refined_goal_text: None,
                    token_count: None,
                    pre_failure_count: Some(0),
                    post_failure_count: None,
                    cap: crate::session::chain_driver::MASTER_IMPROVE_DEFAULT_CAP,
                    started_at: chrono::Utc::now(),
                    ended_at: None,
                };
                let store_guard = store.lock().await;
                if let Err(e) = store_guard.insert_chain_iteration(&iter) {
                    tracing::error!("auto-promote: insert_chain_iteration failed: {e:#}");
                } else {
                    tracing::info!(
                        "auto-promote: chain {} registered for execution {}",
                        chain_id,
                        execution_id
                    );
                }
            });
        }

        Ok(serde_json::to_value(&response)?)
    }

    /// Execute a stored topology through the rsi-graph executor (P1.10).
    ///
    /// Loads the topology by id, validates it is acyclic (loop edges →
    /// `InvalidParam`), bridges it to a `WorkflowDefinition`, then delegates
    /// to `execute_workflow_live`. Acyclic-only until P1.11.
    ///
    /// Returns `{ "execution_id": Uuid }` — poll via `GetWorkflowExecution`.
    pub(super) async fn handle_execute_topology(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ExecuteTopologyParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(e.to_string()))?;

        // P1.12: validate parent_id (if provided) before any executor work.
        self.validate_parent_id(params.parent_id).await?;

        // Load topology from store.
        let topology = self
            .session_manager
            .get_topology(params.topology_id)
            .await?;

        // P1.12 §9: mirror topology → workflows table for gv-picker visibility
        // BEFORE launch. If the upsert fails (e.g. bridge rejects malformed
        // topology), the launch does NOT proceed — a topology that can't
        // surface in gv shouldn't run silently.
        let _ = self
            .session_manager
            .upsert_bridged_workflow(&topology, params.project_id)
            .await?;

        // Bridge topology → WorkflowDefinition (pure, no I/O).
        let workflow_def = self
            .session_manager
            .bridge_topology_to_workflow(&topology)
            .map_err(DaemonError::from)?;

        // Resolve project working_dir if a project_id was supplied.
        let working_dir: Option<std::path::PathBuf> = if let Some(pid) = params.project_id {
            let store = self.session_manager.store().lock().await;
            store
                .get_project(pid)
                .ok()
                .flatten()
                .and_then(|p| p.path)
                .map(std::path::PathBuf::from)
        } else {
            None
        };

        let workflow_id = Uuid::new_v4();
        let input = if params.inputs.is_null() {
            None
        } else {
            Some(params.inputs)
        };

        let response = SessionManager::execute_workflow_live(
            Arc::clone(&self.session_manager),
            workflow_id,
            workflow_def,
            input,
            false, // dry_run: always live for ExecuteTopology
            params.project_id,
            working_dir,
            params.parent_id,
        )
        .await?;

        Ok(serde_json::json!({ "execution_id": response.execution_id }))
    }

    pub(super) async fn handle_start_chained_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: StartChainedWorkflowParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("invalid StartChainedWorkflow params: {e}")))?;

        if params.topology_name != "master_improve" {
            return Err(DaemonError::InvalidParam(format!(
                "topology_name must be 'master_improve', got {:?}",
                params.topology_name
            )));
        }

        let cap = params
            .cap_override
            .unwrap_or(crate::session::chain_driver::MASTER_IMPROVE_DEFAULT_CAP);

        let (chain_id, first_execution_id, accepted_at) =
            crate::session::chain_driver::register_chain(
                Arc::clone(&self.session_manager),
                self.session_manager.store().clone(),
                params.initial_goal,
                cap,
                params.workflow_id,
                params.project_id,
                params.working_dir,
            )
            .await
            .map_err(|e| DaemonError::Rpc(format!("register_chain failed: {e:#}")))?;

        let response = StartChainedWorkflowResponse {
            chain_id,
            first_execution_id,
            accepted_at,
        };
        serde_json::to_value(&response)
            .map_err(|e| DaemonError::Rpc(format!("serialize StartChainedWorkflowResponse: {e}")))
    }

    pub(super) async fn handle_get_workflow_execution(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::GetWorkflowExecutionParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let lookup = self
            .session_manager
            .get_workflow_execution(params.execution_id)
            .await?;
        let response = rsi_common::rpc::GetWorkflowExecutionResponse { lookup };
        Ok(serde_json::to_value(&response)?)
    }

    pub(super) async fn handle_interrupt_workflow_execution(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::InterruptWorkflowExecutionParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let response = self
            .session_manager
            .interrupt_workflow_execution(params.execution_id)
            .await?
            .ok_or_else(|| {
                DaemonError::Store(format!(
                    "Workflow execution not found: {}",
                    params.execution_id
                ))
            })?;
        Ok(serde_json::to_value(&response)?)
    }

    /// Operator-only typed resolution of preserved topology work (#634,
    /// plan §3.4). Absent from every agent catalog; T4 adds the scoped verb.
    pub(super) async fn handle_resolve_topology_attempt(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::ResolveTopologyAttemptParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("invalid params: {e}")))?;
        let response = self
            .session_manager
            .resolve_topology_attempt(&params)
            .await?;
        Ok(serde_json::to_value(&response)?)
    }
}
