use super::*;

pub(super) const DEFAULT_RECURSIVE_LIVE_READ_LIMIT: u32 = 100;

pub(super) const MAX_RECURSIVE_LIVE_READ_LIMIT: u32 = 500;

pub(super) const RECURSIVE_ARTIFACT_SUMMARY_CURSOR_VERSION: u32 = 1;

pub(super) const RECURSIVE_ARTIFACT_SUMMARY_CURSOR_ENDPOINT: &str =
    "ListRecursiveExecutionArtifactSummaries";

pub(super) const RECURSIVE_ARTIFACT_SUMMARY_CURSOR_SORT: &str = "created_at_desc_id_desc";

pub(super) fn validate_recursive_rpc_nonempty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(DaemonError::InvalidParam(format!(
            "Invalid params: {label} must not be empty"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn validate_recursive_rpc_optional_nonempty(
    label: &str,
    value: Option<&str>,
) -> Result<()> {
    if let Some(value) = value {
        validate_recursive_rpc_nonempty(label, value)?;
    }
    Ok(())
}

pub(super) fn topology_recovery_rpc_error(error: DaemonError) -> DaemonError {
    match error {
        DaemonError::Store(message) => DaemonError::InvalidParam(message),
        DaemonError::InvalidParam(message) => DaemonError::InvalidParam(message),
        other => other,
    }
}

pub(super) fn bounded_recursive_live_read_limit(limit: Option<u32>) -> Result<u32> {
    match limit {
        Some(0) => Err(DaemonError::InvalidParam(
            "Invalid params: limit must be positive".to_string(),
        )),
        Some(value) => Ok(value.min(MAX_RECURSIVE_LIVE_READ_LIMIT)),
        None => Ok(DEFAULT_RECURSIVE_LIVE_READ_LIMIT),
    }
}

pub(super) fn recursive_live_output_not_ready_message(
    live_attempt_id: rsi_common::RecursiveLiveAttemptId,
    reason: RecursiveDagLiveOutputNotReadyReason,
) -> String {
    match reason {
        RecursiveDagLiveOutputNotReadyReason::LiveAttemptMissingSession => {
            format!("recursive live attempt {live_attempt_id} has no attached session")
        }
        RecursiveDagLiveOutputNotReadyReason::SessionNotFound => {
            format!("recursive live attempt {live_attempt_id} attached session was not found")
        }
        RecursiveDagLiveOutputNotReadyReason::SessionNotCompleted => {
            format!("recursive live attempt {live_attempt_id} attached session is not Completed")
        }
    }
}

pub(super) fn recursive_graph_not_found_error(graph_id: RecursiveTaskGraphId) -> DaemonError {
    structured_invalid_params(
        "RECURSIVE_GRAPH_NOT_FOUND",
        format!("recursive DAG graph not found: {graph_id}"),
        Some("recursive_task_graph"),
        Some(graph_id.to_string()),
        None,
    )
}

pub(super) fn bounded_recursive_read_page_limit(limit: Option<u32>) -> Result<u32> {
    match limit {
        Some(0) => Err(DaemonError::InvalidParam(
            "Invalid params: limit must be positive".to_string(),
        )),
        Some(value) => Ok(value.min(crate::store::recursive_dag::MAX_RECURSIVE_READ_PAGE_LIMIT)),
        None => Ok(crate::store::recursive_dag::DEFAULT_RECURSIVE_READ_PAGE_LIMIT),
    }
}

#[derive(Debug, Serialize)]
pub(super) struct RecursiveArtifactSummaryCursorFilter {
    endpoint: &'static str,
    graph_id: String,
    task_id: Option<String>,
    attempt_id: Option<String>,
    kind: Option<&'static str>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RecursiveArtifactSummaryCursorToken {
    version: u32,
    endpoint: String,
    sort: String,
    filter_hash: String,
    created_at: DateTime<Utc>,
    artifact_id: i64,
}

pub(super) fn recursive_artifact_kind_wire(kind: RecursiveExecutionArtifactKind) -> &'static str {
    match kind {
        RecursiveExecutionArtifactKind::Inline => "inline",
        RecursiveExecutionArtifactKind::File => "file",
        RecursiveExecutionArtifactKind::SessionEvent => "session_event",
        RecursiveExecutionArtifactKind::WorkflowExecution => "workflow_execution",
    }
}

pub(super) fn recursive_artifact_summary_cursor_filter(
    graph_id: RecursiveTaskGraphId,
    task_id: Option<RecursiveTaskId>,
    attempt_id: Option<RecursiveAttemptId>,
    kind: Option<RecursiveExecutionArtifactKind>,
) -> RecursiveArtifactSummaryCursorFilter {
    RecursiveArtifactSummaryCursorFilter {
        endpoint: RECURSIVE_ARTIFACT_SUMMARY_CURSOR_ENDPOINT,
        graph_id: graph_id.to_string(),
        task_id: task_id.map(|id| id.to_string()),
        attempt_id: attempt_id.map(|id| id.to_string()),
        kind: kind.map(recursive_artifact_kind_wire),
    }
}

pub(super) fn recursive_artifact_summary_filter_hash(
    filter: &RecursiveArtifactSummaryCursorFilter,
) -> Result<String> {
    let bytes = serde_json::to_vec(filter).map_err(DaemonError::Json)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

pub(super) fn decode_recursive_artifact_summary_cursor(
    cursor: &str,
    expected_hash: &str,
) -> Result<crate::store::recursive_dag::RecursiveExecutionArtifactSummaryPageCursor> {
    let token: RecursiveArtifactSummaryCursorToken =
        serde_json::from_str(cursor).map_err(|error| {
            structured_invalid_params(
                "RECURSIVE_CURSOR_INVALID",
                format!("recursive artifact summary cursor is malformed: {error}"),
                Some("recursive_execution_artifact_summary_cursor"),
                None,
                None,
            )
        })?;
    if token.version != RECURSIVE_ARTIFACT_SUMMARY_CURSOR_VERSION
        || token.endpoint != RECURSIVE_ARTIFACT_SUMMARY_CURSOR_ENDPOINT
        || token.sort != RECURSIVE_ARTIFACT_SUMMARY_CURSOR_SORT
        || token.filter_hash != expected_hash
    {
        return Err(structured_invalid_params(
            "RECURSIVE_CURSOR_INVALID",
            "recursive artifact summary cursor does not match this request scope",
            Some("recursive_execution_artifact_summary_cursor"),
            None,
            Some(serde_json::json!({
                "cursor_version": token.version,
                "cursor_endpoint": token.endpoint,
                "cursor_sort": token.sort,
            })),
        ));
    }
    Ok(
        crate::store::recursive_dag::RecursiveExecutionArtifactSummaryPageCursor {
            created_at: token.created_at,
            artifact_id: token.artifact_id,
        },
    )
}

pub(super) fn encode_recursive_artifact_summary_cursor(
    item: &RecursiveExecutionArtifactSummary,
    filter_hash: String,
) -> Result<String> {
    let token = RecursiveArtifactSummaryCursorToken {
        version: RECURSIVE_ARTIFACT_SUMMARY_CURSOR_VERSION,
        endpoint: RECURSIVE_ARTIFACT_SUMMARY_CURSOR_ENDPOINT.to_string(),
        sort: RECURSIVE_ARTIFACT_SUMMARY_CURSOR_SORT.to_string(),
        filter_hash,
        created_at: item.created_at,
        artifact_id: item.artifact_id,
    };
    serde_json::to_string(&token).map_err(DaemonError::Json)
}

pub(super) fn redact_recursive_live_heartbeat_token(
    mut state: RecursiveLiveAttemptHeartbeatState,
) -> RecursiveLiveAttemptHeartbeatState {
    state.heartbeat_token = None;
    state
}

impl RpcServer {
    pub(super) async fn handle_list_recursive_task_graphs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveTaskGraphsParams = if request.params.is_null() {
            ListRecursiveTaskGraphsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        let store = self.session_manager.store().clone();
        let graphs = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.list_recursive_task_graphs(
                crate::store::recursive_dag::RecursiveTaskGraphListFilter {
                    project_id: params.project_id,
                    workflow_id: params.workflow_id,
                    topology_id: params.topology_id,
                    parent_session_id: params.parent_session_id,
                    status: params.status,
                    include_quarantined: params.include_quarantined,
                },
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(graphs)?)
    }

    pub(super) async fn handle_get_recursive_graph_as_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_gv_render_recursive_origin_enabled()?;
        let params: GetRecursiveGraphAsWorkflowParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let store = self.session_manager.store().clone();
        let detail = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_recursive_task_graph(graph_id)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??
        .ok_or_else(|| DaemonError::InvalidParam("recursive graph not found".to_string()))?;

        // Read-only structure bridge (V0 fn reused verbatim; no DB write).
        let definition = self.session_manager.bridge_recursive_to_workflow(
            &detail.graph,
            &detail.nodes,
            &detail.edges,
            &detail.attempts,
        );
        let response = GetRecursiveGraphAsWorkflowResponse {
            definition: serde_json::to_value(&definition)?,
        };
        Ok(serde_json::to_value(response)?)
    }

    pub(super) async fn handle_edit_recursive_node_instructions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_gv_render_recursive_origin_enabled()?;
        let params: EditRecursiveNodeInstructionsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let task_id = RecursiveTaskId(params.task_id);
        let instructions = params.instructions;

        let store = self.session_manager.store().clone();
        let node = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.edit_recursive_node_instructions(graph_id, task_id, instructions)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        Ok(serde_json::to_value(node)?)
    }

    pub(super) async fn handle_edit_recursive_node_settings(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        self.ensure_gv_render_recursive_origin_enabled()?;
        let params: EditRecursiveNodeSettingsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let task_id = RecursiveTaskId(params.task_id);
        let integration = params.integration_strategy;
        let verification = params.verification_strategy;

        let store = self.session_manager.store().clone();
        let node = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.edit_recursive_node_settings(graph_id, task_id, integration, verification)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        Ok(serde_json::to_value(node)?)
    }

    pub(super) async fn handle_list_recursive_graphs_for_topology(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveGraphsForTopologyParams = if request.params.is_null() {
            ListRecursiveGraphsForTopologyParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        validate_recursive_rpc_optional_nonempty("node_id", params.node_id.as_deref())?;
        validate_recursive_rpc_optional_nonempty(
            "execution_owner",
            params.execution_owner.as_deref(),
        )?;

        let store = self.session_manager.store().clone();
        let graphs = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.list_recursive_graphs_for_topology(
                crate::store::recursive_dag::RecursiveTopologyGraphListFilter {
                    graph_id: None,
                    topology_id: params.topology_id,
                    project_id: params.project_id,
                    workflow_id: params.workflow_id,
                    workflow_execution_id: params.workflow_execution_id,
                    node_id: params.node_id,
                    topology_iteration: params.topology_iteration,
                    parent_session_id: params.parent_session_id,
                    execution_owner: params.execution_owner,
                    include_quarantined: params.include_quarantined,
                },
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(graphs)?)
    }

    pub(super) async fn handle_get_topology_recursive_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetTopologyRecursiveStatusParams = if request.params.is_null() {
            GetTopologyRecursiveStatusParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        validate_recursive_rpc_optional_nonempty("node_id", params.node_id.as_deref())?;
        validate_recursive_rpc_optional_nonempty(
            "execution_owner",
            params.execution_owner.as_deref(),
        )?;

        let store = self.session_manager.store().clone();
        let status = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_topology_recursive_status(
                crate::store::recursive_dag::TopologyRecursiveStatusFilter {
                    graph_id: params.graph_id.map(RecursiveTaskGraphId),
                    topology_id: params.topology_id,
                    project_id: params.project_id,
                    workflow_id: params.workflow_id,
                    workflow_execution_id: params.workflow_execution_id,
                    node_id: params.node_id,
                    topology_iteration: params.topology_iteration,
                    parent_session_id: params.parent_session_id,
                    execution_owner: params.execution_owner,
                    include_dynamic_children: params.include_dynamic_children,
                },
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_get_recursive_task_graph(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RecursiveTaskGraphIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let store = self.session_manager.store().clone();
        let graph = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_recursive_task_graph(graph_id)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??
        .ok_or_else(|| {
            DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
        })?;
        Ok(serde_json::to_value(graph)?)
    }

    pub(super) async fn handle_get_recursive_task(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RecursiveTaskIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let task_id = RecursiveTaskId(params.task_id);
        let store = self.session_manager.store().clone();
        let task = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let detail = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            detail
                .nodes
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!("recursive task not found: {task_id}"))
                })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(task)?)
    }

    pub(super) async fn handle_list_recursive_tasks(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveTasksParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let parent_task_id = params.parent_task_id.map(RecursiveTaskId);
        let status = params.status;
        let store = self.session_manager.store().clone();
        let tasks = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let detail = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            Ok::<Vec<_>, DaemonError>(
                detail
                    .nodes
                    .into_iter()
                    .filter(|task| parent_task_id.is_none_or(|id| task.parent_task_id == Some(id)))
                    .filter(|task| status.is_none_or(|status| task.status == status))
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(tasks)?)
    }

    pub(super) async fn handle_list_recursive_task_attempts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveTaskAttemptsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let task_id = params.task_id.map(RecursiveTaskId);
        let status = params.status;
        let store = self.session_manager.store().clone();
        let attempts = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let detail = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            Ok::<Vec<_>, DaemonError>(
                detail
                    .attempts
                    .into_iter()
                    .filter(|attempt| task_id.is_none_or(|id| attempt.task_id == id))
                    .filter(|attempt| status.is_none_or(|status| attempt.status == status))
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(attempts)?)
    }

    pub(super) async fn handle_list_recursive_lifecycle_events(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveLifecycleEventsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let task_id = params.task_id.map(RecursiveTaskId);
        let store = self.session_manager.store().clone();
        let events = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let detail = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            Ok::<Vec<_>, DaemonError>(
                detail
                    .lifecycle_events
                    .into_iter()
                    .filter(|event| task_id.is_none_or(|id| event.task_id == id))
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(events)?)
    }

    pub(super) async fn handle_list_recursive_execution_artifacts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveExecutionArtifactsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let task_id = params.task_id.map(RecursiveTaskId);
        let attempt_id = params.attempt_id.map(RecursiveAttemptId);
        let store = self.session_manager.store().clone();
        let artifacts = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let detail = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            Ok::<Vec<_>, DaemonError>(
                detail
                    .artifacts
                    .into_iter()
                    .filter(|artifact| task_id.is_none_or(|id| artifact.task_id == id))
                    .filter(|artifact| attempt_id.is_none_or(|id| artifact.attempt_id == Some(id)))
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(artifacts)?)
    }

    pub(super) async fn handle_get_recursive_execution_artifact(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveExecutionArtifactParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = params.graph_id;
        let artifact_id = params.artifact_id;
        let store = self.session_manager.store().clone();
        let lookup = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .load_recursive_task_graph_summary(graph_id)?
                .ok_or_else(|| recursive_graph_not_found_error(graph_id))?;
            store.load_recursive_execution_artifact_by_id(graph_id, artifact_id)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        match lookup {
            crate::store::recursive_dag::RecursiveExecutionArtifactLookup::Found(readback) => {
                Ok(serde_json::to_value(readback)?)
            }
            crate::store::recursive_dag::RecursiveExecutionArtifactLookup::NotFound => {
                Err(structured_invalid_params(
                    "RECURSIVE_ARTIFACT_NOT_FOUND",
                    format!("recursive execution artifact not found: {artifact_id}"),
                    Some("recursive_execution_artifact"),
                    Some(artifact_id.to_string()),
                    Some(serde_json::json!({
                        "graph_id": graph_id,
                    })),
                ))
            }
            crate::store::recursive_dag::RecursiveExecutionArtifactLookup::GraphMismatch {
                actual_graph_id,
            } => Err(structured_invalid_params(
                "RECURSIVE_ARTIFACT_GRAPH_MISMATCH",
                format!(
                    "recursive execution artifact {} belongs to graph {}, not {}",
                    artifact_id, actual_graph_id, graph_id
                ),
                Some("recursive_execution_artifact"),
                Some(artifact_id.to_string()),
                Some(serde_json::json!({
                    "requested_graph_id": graph_id,
                    "actual_graph_id": actual_graph_id,
                })),
            )),
        }
    }

    pub(super) async fn handle_preview_recursive_execution_artifact(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: PreviewRecursiveExecutionArtifactParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = params.graph_id;
        let artifact_id = params.artifact_id;
        let require_complete = params.require_complete;
        let max_bytes = if require_complete && params.max_bytes.is_none() {
            Some(crate::store::recursive_dag::MAX_RECURSIVE_ARTIFACT_PREVIEW_MAX_BYTES)
        } else {
            params.max_bytes
        };
        let max_lines = if require_complete && params.max_lines.is_none() {
            Some(crate::store::recursive_dag::MAX_RECURSIVE_ARTIFACT_PREVIEW_MAX_LINES)
        } else {
            params.max_lines
        };
        let options = crate::store::recursive_dag::RecursiveExecutionArtifactPreviewOptions {
            byte_offset: params.byte_offset,
            line_offset: params.line_offset,
            max_bytes,
            max_lines,
        };
        let store = self.session_manager.store().clone();
        let lookup = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .load_recursive_task_graph_summary(graph_id)?
                .ok_or_else(|| recursive_graph_not_found_error(graph_id))?;
            store.preview_recursive_execution_artifact_by_id(graph_id, artifact_id, options)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        match lookup {
            crate::store::recursive_dag::RecursiveExecutionArtifactPreviewLookup::Found(
                preview,
            ) => {
                if require_complete && preview.truncated {
                    return Err(structured_invalid_params(
                        "RECURSIVE_ARTIFACT_OVERSIZED",
                        format!(
                            "recursive execution artifact {artifact_id} exceeds preview caps"
                        ),
                        Some("recursive_execution_artifact"),
                        Some(artifact_id.to_string()),
                        Some(serde_json::json!({
                            "graph_id": graph_id,
                            "total_bytes": preview.total_bytes,
                            "total_lines": preview.total_lines,
                            "applied_max_bytes": preview.applied_max_bytes,
                            "applied_max_lines": preview.applied_max_lines,
                            "truncated_by_bytes": preview.truncated_by_bytes,
                            "truncated_by_lines": preview.truncated_by_lines,
                        })),
                    ));
                }
                Ok(serde_json::to_value(preview)?)
            }
            crate::store::recursive_dag::RecursiveExecutionArtifactPreviewLookup::NotFound => {
                Err(structured_invalid_params(
                    "RECURSIVE_ARTIFACT_NOT_FOUND",
                    format!("recursive execution artifact not found: {artifact_id}"),
                    Some("recursive_execution_artifact"),
                    Some(artifact_id.to_string()),
                    Some(serde_json::json!({
                        "graph_id": graph_id,
                    })),
                ))
            }
            crate::store::recursive_dag::RecursiveExecutionArtifactPreviewLookup::GraphMismatch {
                actual_graph_id,
            } => Err(structured_invalid_params(
                "RECURSIVE_ARTIFACT_GRAPH_MISMATCH",
                format!(
                    "recursive execution artifact {} belongs to graph {}, not {}",
                    artifact_id, actual_graph_id, graph_id
                ),
                Some("recursive_execution_artifact"),
                Some(artifact_id.to_string()),
                Some(serde_json::json!({
                    "requested_graph_id": graph_id,
                    "actual_graph_id": actual_graph_id,
                })),
            )),
        }
    }

    pub(super) async fn handle_list_recursive_execution_artifact_summaries(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveExecutionArtifactSummariesParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let Some(graph_id) = params.graph_id else {
            return Err(structured_invalid_params(
                "INVALID_PARAMS",
                "Invalid params: graph_id is required for artifact summary readback",
                Some("recursive_task_graph"),
                None,
                None,
            ));
        };
        if params.live_attempt_id.is_some()
            || params.scheduler_run_id.is_some()
            || params.validation_id.is_some()
            || params.role.is_some()
        {
            return Err(structured_invalid_params(
                "INVALID_PARAMS",
                "Invalid params: artifact summary pagination currently supports graph_id with optional task_id, attempt_id, and kind filters only",
                Some("recursive_execution_artifact_summary_filter"),
                None,
                Some(serde_json::json!({
                    "live_attempt_id_supported": false,
                    "scheduler_run_id_supported": false,
                    "validation_id_supported": false,
                    "role_supported": false,
                })),
            ));
        }

        let limit = bounded_recursive_read_page_limit(params.limit)?;
        let cursor_filter = recursive_artifact_summary_cursor_filter(
            graph_id,
            params.task_id,
            params.attempt_id,
            params.kind,
        );
        let filter_hash = recursive_artifact_summary_filter_hash(&cursor_filter)?;
        let after = params
            .cursor
            .as_deref()
            .map(|cursor| decode_recursive_artifact_summary_cursor(cursor, &filter_hash))
            .transpose()?;
        let store_filter =
            crate::store::recursive_dag::RecursiveExecutionArtifactSummaryListFilter {
                graph_id,
                task_id: params.task_id,
                attempt_id: params.attempt_id,
                kind: params.kind,
            };
        let page_options =
            crate::store::recursive_dag::RecursiveExecutionArtifactSummaryPageOptions {
                limit,
                after,
                include_total: params.include_total,
            };
        let store = self.session_manager.store().clone();
        let mut page: RecursiveReadPage<RecursiveExecutionArtifactSummary> =
            tokio::task::spawn_blocking(move || {
                let store = store.blocking_lock();
                store
                    .load_recursive_task_graph_summary(graph_id)?
                    .ok_or_else(|| recursive_graph_not_found_error(graph_id))?;
                store.list_recursive_execution_artifact_summaries(store_filter, page_options)
            })
            .await
            .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;

        if page.has_more
            && let Some(last) = page.items.last()
        {
            page.next_cursor = Some(encode_recursive_artifact_summary_cursor(last, filter_hash)?);
        }
        Ok(serde_json::to_value(page)?)
    }

    pub(super) async fn handle_list_recursive_scheduler_runs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveSchedulerRunsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let store = self.session_manager.store().clone();
        let runs = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            let mut runs = store.list_recursive_scheduler_runs_for_graph(graph_id)?;
            runs.retain(|run| {
                params.status.is_none_or(|status| run.status == status)
                    && params.source.is_none_or(|source| run.source == source)
                    && (params.include_terminal
                        || matches!(
                            run.status,
                            RecursiveSchedulerRunStatus::Running
                                | RecursiveSchedulerRunStatus::Cancelling
                        ))
            });
            if let Some(limit) = params.limit {
                runs.truncate(limit as usize);
            }
            Ok::<_, DaemonError>(runs)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(runs)?)
    }

    pub(super) async fn handle_get_recursive_scheduler_run(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RecursiveSchedulerRunIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let run_id = RecursiveSchedulerRunId(params.run_id);
        let store = self.session_manager.store().clone();
        let detail = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let run = store.load_recursive_scheduler_run(run_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive scheduler run not found: {run_id}"))
            })?;
            let graph_detail = store
                .get_recursive_task_graph(run.graph_id)?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive DAG graph not found for scheduler run {run_id}: {}",
                        run.graph_id
                    ))
                })?;
            let events = store.list_recursive_scheduler_run_events(run_id, None, Some(200))?;
            let cancellation_requests = store.list_recursive_cancellation_requests(
                crate::store::recursive_dag::RecursiveCancellationRequestFilter {
                    graph_id: None,
                    run_id: Some(run_id),
                    status: None,
                    limit: None,
                },
            )?;
            let active_attempts = graph_detail
                .attempts
                .iter()
                .filter(|attempt| attempt.status == RecursiveAttemptStatus::Running)
                .cloned()
                .collect();
            let report_artifact = run.report_artifact_id.and_then(|artifact_id| {
                graph_detail
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.id == artifact_id)
                    .cloned()
            });
            let live_attempt_details =
                store.list_recursive_live_attempts_for_scheduler_run(run_id)?;
            let live_attempt_ids = live_attempt_details
                .iter()
                .map(|live| live.summary.id)
                .collect::<Vec<_>>();
            let latest_live_validations = store
                .latest_recursive_live_output_validation_summaries_for_live_attempts(
                    &live_attempt_ids,
                )?
                .into_values()
                .collect();
            let mut live_heartbeat_states = Vec::new();
            let mut live_interrupts = Vec::new();
            for live in &live_attempt_details {
                if let Some(state) = store.load_recursive_live_attempt_heartbeat(live.summary.id)? {
                    live_heartbeat_states.push(redact_recursive_live_heartbeat_token(state));
                }
                live_interrupts.extend(
                    store.list_recursive_live_interrupts_for_live_attempt(live.summary.id)?,
                );
            }
            let live_attempts = live_attempt_details
                .into_iter()
                .map(|live| live.summary)
                .collect();
            Ok::<_, DaemonError>(rsi_common::RecursiveSchedulerRunDetail {
                run,
                graph: graph_detail.graph,
                active_attempts,
                cancellation_requests,
                events,
                report_artifact,
                live_attempts,
                live_heartbeat_states,
                live_interrupts,
                latest_live_validations,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(detail)?)
    }

    pub(super) async fn handle_list_recursive_scheduler_run_events(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveSchedulerRunEventsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        if params.since_id.is_some_and(|since_id| since_id < 0) {
            return Err(DaemonError::InvalidParam(
                "Invalid params: since_id must be non-negative".to_string(),
            ));
        }
        let run_id = RecursiveSchedulerRunId(params.run_id);
        let store = self.session_manager.store().clone();
        let events = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_recursive_scheduler_run(run_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive scheduler run not found: {run_id}"))
            })?;
            store.list_recursive_scheduler_run_events(run_id, params.since_id, params.limit)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(events)?)
    }

    pub(super) async fn handle_get_recursive_dag_operational_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RecursiveTaskGraphIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let graph_id = RecursiveTaskGraphId(params.graph_id);
        let store = self.session_manager.store().clone();
        let status = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let detail = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
            })?;
            let runs = store.list_recursive_scheduler_runs_for_graph(graph_id)?;
            let active_run = runs
                .iter()
                .find(|run| {
                    matches!(
                        run.status,
                        RecursiveSchedulerRunStatus::Running
                            | RecursiveSchedulerRunStatus::Cancelling
                    )
                })
                .cloned();
            let latest_run = runs.first().cloned();
            let open_cancellation_requests =
                store.list_pending_recursive_cancellation_requests_for_graph(graph_id)?;
            let running_attempts = detail
                .attempts
                .iter()
                .filter(|attempt| attempt.status == RecursiveAttemptStatus::Running)
                .cloned()
                .collect();
            let recovery = store
                .load_recursive_recovery_graph_status(graph_id)?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive recovery graph status not found: {graph_id}"
                    ))
                })?;
            let live_attempt_details = store.list_recursive_live_attempts_for_graph(graph_id)?;
            let mut active_live_attempts = Vec::new();
            let mut live_recovery_pending = Vec::new();
            for live in &live_attempt_details {
                if !live.summary.status.is_terminal()
                    || matches!(
                        live.summary.status,
                        RecursiveLiveAttemptStatus::Lost
                            | RecursiveLiveAttemptStatus::RecoveryPending
                    )
                    || matches!(
                        live.summary.recovery_status,
                        rsi_common::RecursiveLiveRecoveryStatus::Pending
                            | rsi_common::RecursiveLiveRecoveryStatus::Lost
                            | rsi_common::RecursiveLiveRecoveryStatus::Quarantined
                    )
                {
                    active_live_attempts.push(live.summary.clone());
                }
                if matches!(
                    live.summary.recovery_status,
                    rsi_common::RecursiveLiveRecoveryStatus::Pending
                        | rsi_common::RecursiveLiveRecoveryStatus::Lost
                        | rsi_common::RecursiveLiveRecoveryStatus::Quarantined
                ) {
                    live_recovery_pending.push(live.summary.clone());
                }
            }
            let live_attempt_ids = active_live_attempts
                .iter()
                .map(|live| live.id)
                .collect::<Vec<_>>();
            let latest_live_validations = store
                .latest_recursive_live_output_validation_summaries_for_live_attempts(
                    &live_attempt_ids,
                )?
                .into_values()
                .collect();
            let mut active_live_heartbeats = Vec::new();
            let mut active_live_interrupts = Vec::new();
            for live in &active_live_attempts {
                if let Some(state) = store.load_recursive_live_attempt_heartbeat(live.id)? {
                    active_live_heartbeats.push(redact_recursive_live_heartbeat_token(state));
                }
                if let Some(interrupt) =
                    store.load_active_recursive_live_interrupt_for_attempt(live.id)?
                {
                    active_live_interrupts.push(interrupt);
                }
            }
            Ok::<_, DaemonError>(rsi_common::RecursiveDagOperationalStatus {
                graph: detail.graph,
                active_run,
                latest_run,
                open_cancellation_requests,
                running_attempts,
                recovery,
                active_live_attempts,
                active_live_heartbeats,
                active_live_interrupts,
                live_recovery_pending,
                latest_live_validations,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_get_recursive_live_attempt(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveLiveAttemptParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().clone();
        let readback = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let live = store
                .load_recursive_live_attempt(params.live_attempt_id)?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive live attempt not found: {}",
                        params.live_attempt_id
                    ))
                })?;
            let graph = store
                .get_recursive_task_graph(live.summary.graph_id)?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive DAG graph not found for live attempt {}: {}",
                        live.summary.id, live.summary.graph_id
                    ))
                })?;
            let task = graph
                .nodes
                .iter()
                .find(|task| task.id == live.summary.task_id)
                .cloned()
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
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
                    DaemonError::InvalidParam(format!(
                        "recursive attempt not found for live attempt {}: {}",
                        live.summary.id, live.summary.attempt_id
                    ))
                })?;
            let scheduler_run = store
                .load_recursive_scheduler_run(live.summary.scheduler_run_id)?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive scheduler run not found for live attempt {}: {}",
                        live.summary.id, live.summary.scheduler_run_id
                    ))
                })?;

            let mut warnings = Vec::new();
            let session = if params.include_session {
                match live.summary.session_id {
                    Some(session_id) => {
                        let session =
                            store.load_recursive_live_linked_session_summary(session_id)?;
                        if session.is_none() {
                            warnings.push(rsi_common::RecursiveReadbackWarning {
                                code: "linked_session_missing".to_string(),
                                message: format!("linked session not found: {session_id}"),
                                resource_type: Some("session".to_string()),
                                resource_id: Some(session_id.to_string()),
                            });
                        }
                        session
                    }
                    None => None,
                }
            } else {
                None
            };
            let heartbeat = if params.include_heartbeat {
                store
                    .load_recursive_live_attempt_heartbeat(live.summary.id)?
                    .map(redact_recursive_live_heartbeat_token)
            } else {
                None
            };
            let latest_interrupt = if params.include_interrupt {
                store.load_existing_recursive_live_interrupt_for_attempt(live.summary.id)?
            } else {
                None
            };
            let latest_validation = if params.include_validation {
                store
                    .load_latest_recursive_live_output_validation_result_for_live_attempt(
                        live.summary.id,
                        crate::store::recursive_dag::RecursiveLiveOutputValidationReadOptions::default(),
                    )?
                    .map(|result| result.summary)
            } else {
                None
            };
            let artifacts = if params.include_artifacts {
                store.load_recursive_live_attempt_artifacts(
                    live.summary.id,
                    crate::store::recursive_dag::RecursiveLiveAttemptArtifactReadOptions {
                        include_prompt: true,
                        include_raw_output: false,
                        include_normalized_output: true,
                        include_validation_report: false,
                        include_diff: true,
                        include_tests: true,
                        include_produced_artifacts: true,
                    },
                )?
            } else {
                None
            };
            let retry_history = if params.include_retry_history {
                Some(
                    graph
                        .attempts
                        .iter()
                        .filter(|attempt| attempt.task_id == live.summary.task_id)
                        .cloned()
                        .collect(),
                )
            } else {
                None
            };

            Ok::<_, DaemonError>(rsi_common::RecursiveLiveAttemptReadback {
                live_attempt: live,
                task,
                recursive_attempt,
                scheduler_run,
                session,
                heartbeat,
                latest_interrupt,
                latest_validation,
                artifacts,
                retry_history,
                warnings,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(readback)?)
    }

    pub(super) async fn handle_list_recursive_live_attempts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveLiveAttemptsParams = if request.params.is_null() {
            ListRecursiveLiveAttemptsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        if params.task_id.is_some() && params.graph_id.is_none() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: task_id requires graph_id".to_string(),
            ));
        }
        let anchor_count = usize::from(params.graph_id.is_some())
            + usize::from(params.scheduler_run_id.is_some())
            + usize::from(params.session_id.is_some());
        if anchor_count != 1 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one of graph_id, scheduler_run_id, or session_id is required".to_string(),
            ));
        }
        let limit = bounded_recursive_live_read_limit(params.limit)?;
        let store = self.session_manager.store().clone();
        let items = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(graph_id) = params.graph_id {
                let graph = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
                })?;
                if let Some(task_id) = params.task_id
                    && !graph.nodes.iter().any(|task| task.id == task_id)
                {
                    return Err(DaemonError::InvalidParam(format!(
                        "recursive task not found: {task_id}"
                    )));
                }
            }
            if let Some(run_id) = params.scheduler_run_id {
                store.load_recursive_scheduler_run(run_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive scheduler run not found: {run_id}"
                    ))
                })?;
            }
            if let Some(session_id) = params.session_id {
                store.get_session(session_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("session not found: {session_id}"))
                })?;
            }
            let attempts = store.list_recursive_live_attempts(
                crate::store::recursive_dag::RecursiveLiveAttemptListFilter {
                    graph_id: params.graph_id,
                    task_id: params.task_id,
                    scheduler_run_id: params.scheduler_run_id,
                    session_id: params.session_id,
                    status: params.status,
                    recovery_status: params.recovery_status,
                    include_terminal: params.include_terminal,
                    limit: Some(limit),
                },
            )?;
            let live_attempt_ids = attempts
                .iter()
                .map(|attempt| attempt.summary.id)
                .collect::<Vec<_>>();
            let latest_validations = if params.include_status {
                store.latest_recursive_live_output_validation_summaries_for_live_attempts(
                    &live_attempt_ids,
                )?
            } else {
                Default::default()
            };
            attempts
                .into_iter()
                .map(|attempt| {
                    let heartbeat = if params.include_status {
                        store
                            .load_recursive_live_attempt_heartbeat(attempt.summary.id)?
                            .map(redact_recursive_live_heartbeat_token)
                    } else {
                        None
                    };
                    let latest_interrupt = if params.include_status {
                        store.load_existing_recursive_live_interrupt_for_attempt(
                            attempt.summary.id,
                        )?
                    } else {
                        None
                    };
                    Ok(rsi_common::RecursiveLiveAttemptListItem {
                        latest_validation: latest_validations.get(&attempt.summary.id).cloned(),
                        summary: attempt.summary,
                        heartbeat,
                        latest_interrupt,
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(items)?)
    }

    pub(super) async fn handle_get_recursive_live_attempt_heartbeat_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveLiveAttemptHeartbeatStatusParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().clone();
        let heartbeat = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .load_recursive_live_attempt_heartbeat(params.live_attempt_id)?
                .map(redact_recursive_live_heartbeat_token)
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive live attempt not found: {}",
                        params.live_attempt_id
                    ))
                })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(heartbeat)?)
    }

    pub(super) async fn handle_list_stale_recursive_live_attempt_heartbeats(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListStaleRecursiveLiveAttemptHeartbeatsParams = if request.params.is_null() {
            ListStaleRecursiveLiveAttemptHeartbeatsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        let limit = bounded_recursive_live_read_limit(params.limit)?;
        let store = self.session_manager.store().clone();
        let heartbeats = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let mut heartbeats = if let Some(graph_id) = params.graph_id {
                store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
                })?;
                store.list_stale_recursive_live_attempt_heartbeats_for_graph(graph_id)?
            } else {
                store.list_stale_recursive_live_attempt_heartbeats()?
            };
            heartbeats.truncate(limit as usize);
            Ok::<Vec<_>, DaemonError>(
                heartbeats
                    .into_iter()
                    .map(redact_recursive_live_heartbeat_token)
                    .collect(),
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(heartbeats)?)
    }

    pub(super) async fn handle_get_recursive_live_interrupt_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveLiveInterruptStatusParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        if params.interrupt_id.is_some() == params.live_attempt_id.is_some() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one of interrupt_id or live_attempt_id is required"
                    .to_string(),
            ));
        }
        let store = self.session_manager.store().clone();
        let interrupt = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(interrupt_id) = params.interrupt_id {
                store
                    .load_recursive_live_interrupt(interrupt_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live interrupt not found: {interrupt_id}"
                        ))
                    })
                    .map(Some)
            } else {
                let live_attempt_id = params.live_attempt_id.expect("checked above");
                store
                    .load_recursive_live_attempt(live_attempt_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live attempt not found: {live_attempt_id}"
                        ))
                    })?;
                store.load_existing_recursive_live_interrupt_for_attempt(live_attempt_id)
            }
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(interrupt)?)
    }

    pub(super) async fn handle_list_recursive_live_interrupts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveLiveInterruptsParams = if request.params.is_null() {
            ListRecursiveLiveInterruptsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        let anchor_count = usize::from(params.live_attempt_id.is_some())
            + usize::from(params.cancellation_request_id.is_some())
            + usize::from(params.session_id.is_some());
        if anchor_count != 1 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one of live_attempt_id, cancellation_request_id, or session_id is required".to_string(),
            ));
        }
        let limit = bounded_recursive_live_read_limit(params.limit)?;
        let store = self.session_manager.store().clone();
        let interrupts = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(live_attempt_id) = params.live_attempt_id {
                store
                    .load_recursive_live_attempt(live_attempt_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live attempt not found: {live_attempt_id}"
                        ))
                    })?;
            }
            if let Some(request_id) = params.cancellation_request_id {
                store
                    .load_recursive_cancellation_request(request_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive cancellation request not found: {request_id}"
                        ))
                    })?;
            }
            if let Some(session_id) = params.session_id {
                store.get_session(session_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("session not found: {session_id}"))
                })?;
            }
            store.list_recursive_live_interrupts(
                crate::store::recursive_dag::RecursiveLiveInterruptListFilter {
                    live_attempt_id: params.live_attempt_id,
                    cancellation_request_id: params.cancellation_request_id,
                    session_id: params.session_id,
                    status: params.status,
                    limit: Some(limit),
                },
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(interrupts)?)
    }

    pub(super) async fn handle_get_recursive_live_recovery_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveLiveRecoveryStatusParams = if request.params.is_null() {
            GetRecursiveLiveRecoveryStatusParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        let anchor_count = usize::from(params.graph_id.is_some())
            + usize::from(params.live_attempt_id.is_some())
            + usize::from(params.session_id.is_some());
        if anchor_count != 1 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one of graph_id, live_attempt_id, or session_id is required".to_string(),
            ));
        }
        let store = self.session_manager.store().clone();
        let readback = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let mut warnings = Vec::new();
            let (graph_id, mut live_attempts) = if let Some(live_attempt_id) =
                params.live_attempt_id
            {
                let live = store
                    .load_recursive_live_attempt(live_attempt_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live attempt not found: {live_attempt_id}"
                        ))
                    })?;
                (Some(live.summary.graph_id), vec![live])
            } else if let Some(session_id) = params.session_id {
                store.get_session(session_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("session not found: {session_id}"))
                })?;
                match store.load_recursive_live_attempt_by_session_id(session_id)? {
                    Some(live) => (Some(live.summary.graph_id), vec![live]),
                    None => {
                        warnings.push(rsi_common::RecursiveReadbackWarning {
                            code: "session_has_no_recursive_live_attempt".to_string(),
                            message: format!(
                                "session has no linked recursive live attempt: {session_id}"
                            ),
                            resource_type: Some("session".to_string()),
                            resource_id: Some(session_id.to_string()),
                        });
                        (None, Vec::new())
                    }
                }
            } else {
                let graph_id = params.graph_id.expect("checked above");
                store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
                })?;
                (
                    Some(graph_id),
                    store.list_recursive_live_attempts_for_graph(graph_id)?,
                )
            };
            if !params.include_recovery_pending {
                live_attempts.retain(|live| {
                    live.summary.status != RecursiveLiveAttemptStatus::RecoveryPending
                        && live.summary.recovery_status
                            != rsi_common::RecursiveLiveRecoveryStatus::Pending
                });
            }
            let graph_recovery = graph_id
                .map(|graph_id| store.load_recursive_recovery_graph_status(graph_id))
                .transpose()?
                .flatten();
            let scheduler_runs = graph_id
                .map(|graph_id| store.list_recursive_scheduler_runs_for_graph(graph_id))
                .transpose()?
                .unwrap_or_default();
            let deferred_graph = if params.include_deferred_graph {
                match graph_id {
                    Some(graph_id) => store
                        .list_deferred_recursive_recovery_graphs()?
                        .into_iter()
                        .find(|deferred| deferred.graph_id == Some(graph_id)),
                    None => None,
                }
            } else {
                None
            };
            let mut heartbeat_states = Vec::new();
            for live in &live_attempts {
                if let Some(state) = store.load_recursive_live_attempt_heartbeat(live.summary.id)? {
                    heartbeat_states.push(redact_recursive_live_heartbeat_token(state));
                }
            }
            let mut linked_sessions = Vec::new();
            for session_id in live_attempts
                .iter()
                .filter_map(|live| live.summary.session_id)
            {
                match store.load_recursive_live_linked_session_summary(session_id)? {
                    Some(session) => linked_sessions.push(session),
                    None => warnings.push(rsi_common::RecursiveReadbackWarning {
                        code: "linked_session_missing".to_string(),
                        message: format!("linked session not found: {session_id}"),
                        resource_type: Some("session".to_string()),
                        resource_id: Some(session_id.to_string()),
                    }),
                }
            }
            let operator_review_required = deferred_graph.is_some()
                || graph_recovery.as_ref().is_some_and(|status| {
                    matches!(
                        status.state,
                        rsi_common::RecursiveGraphRecoveryState::Deferred
                            | rsi_common::RecursiveGraphRecoveryState::Quarantined
                    )
                })
                || live_attempts.iter().any(|live| {
                    matches!(
                        live.summary.status,
                        RecursiveLiveAttemptStatus::RecoveryPending
                            | RecursiveLiveAttemptStatus::Lost
                    ) || matches!(
                        live.summary.recovery_status,
                        rsi_common::RecursiveLiveRecoveryStatus::Pending
                            | rsi_common::RecursiveLiveRecoveryStatus::Lost
                            | rsi_common::RecursiveLiveRecoveryStatus::Quarantined
                    )
                });
            Ok::<_, DaemonError>(rsi_common::RecursiveLiveRecoveryReadback {
                graph_recovery,
                live_attempts,
                heartbeat_states,
                scheduler_runs,
                linked_sessions,
                deferred_graph,
                operator_review_required,
                warnings,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(readback)?)
    }

    pub(super) async fn handle_get_recursive_live_output_validation_result(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveLiveOutputValidationResultParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        if params.validation_id.is_some() == params.live_attempt_id.is_some() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one of validation_id or live_attempt_id is required"
                    .to_string(),
            ));
        }
        if params.validation_id.is_some() && !params.latest {
            return Err(DaemonError::InvalidParam(
                "Invalid params: latest=false is invalid with validation_id".to_string(),
            ));
        }
        if params.live_attempt_id.is_some() && !params.latest {
            return Err(DaemonError::InvalidParam(
                "Invalid params: latest=false with live_attempt_id requires ListRecursiveLiveOutputValidationResults".to_string(),
            ));
        }
        let options = crate::store::recursive_dag::RecursiveLiveOutputValidationReadOptions {
            include_issues: params.include_issues,
            include_normalized_output: params.include_normalized_output,
            include_validation_report: params.include_validation_report,
        };
        let store = self.session_manager.store().clone();
        let result = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(validation_id) = params.validation_id {
                store
                    .load_recursive_live_output_validation_result(validation_id, options)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live output validation not found: {validation_id}"
                        ))
                    })
                    .map(Some)
            } else {
                let live_attempt_id = params.live_attempt_id.expect("checked above");
                store
                    .load_recursive_live_attempt(live_attempt_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live attempt not found: {live_attempt_id}"
                        ))
                    })?;
                store.load_latest_recursive_live_output_validation_result_for_live_attempt(
                    live_attempt_id,
                    options,
                )
            }
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_recursive_live_output_validation_results(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveLiveOutputValidationResultsParams = if request.params.is_null() {
            ListRecursiveLiveOutputValidationResultsParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        if params.task_id.is_some() && params.graph_id.is_none() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: task_id requires graph_id".to_string(),
            ));
        }
        let anchor_count = usize::from(params.graph_id.is_some())
            + usize::from(params.scheduler_run_id.is_some())
            + usize::from(params.attempt_id.is_some())
            + usize::from(params.live_attempt_id.is_some())
            + usize::from(params.session_id.is_some());
        if anchor_count != 1 {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one validation list anchor is required".to_string(),
            ));
        }
        let limit = bounded_recursive_live_read_limit(params.limit)?;
        let store = self.session_manager.store().clone();
        let items = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(graph_id) = params.graph_id {
                let graph = store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
                })?;
                if let Some(task_id) = params.task_id
                    && !graph.nodes.iter().any(|task| task.id == task_id)
                {
                    return Err(DaemonError::InvalidParam(format!(
                        "recursive task not found: {task_id}"
                    )));
                }
            }
            if let Some(run_id) = params.scheduler_run_id {
                store.load_recursive_scheduler_run(run_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive scheduler run not found: {run_id}"
                    ))
                })?;
            }
            if let Some(live_attempt_id) = params.live_attempt_id {
                store
                    .load_recursive_live_attempt(live_attempt_id)?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live attempt not found: {live_attempt_id}"
                        ))
                    })?;
            }
            if let Some(attempt_id) = params.attempt_id
                && !store.recursive_task_attempt_exists(attempt_id)?
            {
                return Err(DaemonError::InvalidParam(format!(
                    "recursive attempt not found: {attempt_id}"
                )));
            }
            if let Some(session_id) = params.session_id {
                store.get_session(session_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("session not found: {session_id}"))
                })?;
            }
            let results = store.list_recursive_live_output_validation_results(
                crate::store::recursive_dag::RecursiveLiveOutputValidationListFilter {
                    graph_id: params.graph_id,
                    task_id: params.task_id,
                    scheduler_run_id: params.scheduler_run_id,
                    attempt_id: params.attempt_id,
                    live_attempt_id: params.live_attempt_id,
                    session_id: params.session_id,
                    status: params.status,
                    output_kind: params.output_kind,
                    include_issues: params.include_issues,
                    limit: Some(limit),
                },
            )?;
            Ok::<_, DaemonError>(
                results
                    .into_iter()
                    .map(|result| rsi_common::RecursiveLiveOutputValidationListItem {
                        summary: result.summary,
                        artifact_links: result.artifact_links,
                        issues: params.include_issues.then_some(result.issues),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(items)?)
    }

    pub(super) async fn handle_list_recursive_live_validation_issues(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveLiveValidationIssuesParams = if request.params.is_null() {
            ListRecursiveLiveValidationIssuesParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        if params.validation_id.is_some() == params.live_attempt_id.is_some() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: exactly one of validation_id or live_attempt_id is required"
                    .to_string(),
            ));
        }
        let limit = bounded_recursive_live_read_limit(params.limit)?;
        let store = self.session_manager.store().clone();
        let issues = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(validation_id) = params.validation_id {
                store
                    .load_recursive_live_output_validation_result(
                        validation_id,
                        crate::store::recursive_dag::RecursiveLiveOutputValidationReadOptions::default(),
                    )?
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "recursive live output validation not found: {validation_id}"
                        ))
                    })?;
            }
            if let Some(live_attempt_id) = params.live_attempt_id {
                store.load_recursive_live_attempt(live_attempt_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive live attempt not found: {live_attempt_id}"
                    ))
                })?;
            }
            store.list_recursive_live_validation_issues(
                crate::store::recursive_dag::RecursiveLiveValidationIssueFilter {
                    validation_id: params.validation_id,
                    live_attempt_id: params.live_attempt_id,
                    severity: params.severity,
                    class: params.class,
                    code: params.code,
                    limit: Some(limit),
                },
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(issues)?)
    }

    pub(super) async fn handle_get_recursive_live_attempt_artifacts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveLiveAttemptArtifactsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().clone();
        let artifacts = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .load_recursive_live_attempt_artifacts(
                    params.live_attempt_id,
                    crate::store::recursive_dag::RecursiveLiveAttemptArtifactReadOptions {
                        include_prompt: params.include_prompt,
                        include_raw_output: params.include_raw_output,
                        include_normalized_output: params.include_normalized_output,
                        include_validation_report: params.include_validation_report,
                        include_diff: params.include_diff,
                        include_tests: params.include_tests,
                        include_produced_artifacts: params.include_produced_artifacts,
                    },
                )?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive live attempt not found: {}",
                        params.live_attempt_id
                    ))
                })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(artifacts)?)
    }

    pub(super) async fn handle_list_recursive_cancellation_requests(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListRecursiveCancellationRequestsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        if params.graph_id.is_none() && params.run_id.is_none() {
            return Err(DaemonError::InvalidParam(
                "Invalid params: graph_id or run_id is required".to_string(),
            ));
        }
        let graph_id = params.graph_id.map(RecursiveTaskGraphId);
        let run_id = params.run_id.map(RecursiveSchedulerRunId);
        let store = self.session_manager.store().clone();
        let requests = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            if let Some(graph_id) = graph_id {
                store.get_recursive_task_graph(graph_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!("recursive DAG graph not found: {graph_id}"))
                })?;
            }
            if let Some(run_id) = run_id {
                let run = store.load_recursive_scheduler_run(run_id)?.ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive scheduler run not found: {run_id}"
                    ))
                })?;
                if let Some(graph_id) = graph_id
                    && run.graph_id != graph_id
                {
                    return Err(DaemonError::InvalidParam(format!(
                        "recursive scheduler run {run_id} belongs to graph {}, not {graph_id}",
                        run.graph_id
                    )));
                }
            }
            store.list_recursive_cancellation_requests(
                crate::store::recursive_dag::RecursiveCancellationRequestFilter {
                    graph_id,
                    run_id,
                    status: params.status,
                    limit: params.limit,
                },
            )
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(requests)?)
    }

    pub(super) async fn handle_get_recursive_cancellation_request(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RecursiveCancellationRequestIdParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;
        let request_id = RecursiveCancellationRequestId(params.request_id);
        let store = self.session_manager.store().clone();
        let cancellation = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .load_recursive_cancellation_request(request_id)?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(format!(
                        "recursive cancellation request not found: {request_id}"
                    ))
                })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(cancellation)?)
    }

    pub(super) async fn handle_get_recursive_recovery_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetRecursiveRecoveryStatusParams = if request.params.is_null() {
            GetRecursiveRecoveryStatusParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?
        };
        let graph_id = params.graph_id.map(RecursiveTaskGraphId);
        let store = self.session_manager.store().clone();
        let status = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let graph_status = if let Some(graph_id) = graph_id {
                Some(
                    store
                        .load_recursive_recovery_graph_status(graph_id)?
                        .ok_or_else(|| {
                            DaemonError::InvalidParam(format!(
                                "recursive recovery graph status not found: {graph_id}"
                            ))
                        })?,
                )
            } else {
                None
            };
            let deferred_graphs = store.list_deferred_recursive_recovery_graphs()?;
            let oldest_deferred_graph = deferred_graphs.first().cloned();
            Ok::<_, DaemonError>(rsi_common::RecursiveDagRecoveryStatus {
                latest_pass: store.load_latest_recursive_recovery_pass()?,
                graph_status,
                deferred_graph_count: deferred_graphs.len() as u64,
                deferred_graphs,
                oldest_deferred_graph,
            })
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_list_recursive_deferred_recovery_graphs(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let store = self.session_manager.store().clone();
        let deferred = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.list_deferred_recursive_recovery_graphs()
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Task join error: {}", e)))??;
        Ok(serde_json::to_value(deferred)?)
    }
}
