use super::*;

pub(crate) fn link_issue_rpc_error(error: &IdeaControlError) -> DaemonError {
    let (rpc_code, code, message, details) = match error {
        IdeaControlError::InvalidRequest(_) => (
            INVALID_PARAMS,
            "ISSUE_LINK_INVALID_REQUEST",
            "invalid Issue link request",
            serde_json::Value::Null,
        ),
        IdeaControlError::IssueNotFound => (
            INVALID_PARAMS,
            "ISSUE_NOT_FOUND",
            "Issue not found",
            serde_json::Value::Null,
        ),
        IdeaControlError::IdeaNotFound => (
            INVALID_PARAMS,
            "IDEA_NOT_FOUND",
            "Idea not found",
            serde_json::Value::Null,
        ),
        IdeaControlError::ProjectScopeMismatch => (
            INVALID_PARAMS,
            "ISSUE_LINK_PROJECT_SCOPE_MISMATCH",
            "Issue link project scope mismatch",
            serde_json::Value::Null,
        ),
        IdeaControlError::SourceEventScopeMismatch => (
            INVALID_PARAMS,
            "ISSUE_LINK_SOURCE_EVENT_MISMATCH",
            "Issue link source event mismatch",
            serde_json::Value::Null,
        ),
        IdeaControlError::IssueAlreadyLinked => (
            INVALID_PARAMS,
            "ISSUE_ALREADY_LINKED",
            "Issue is already linked",
            serde_json::Value::Null,
        ),
        IdeaControlError::StaleVersion { expected, actual } => (
            INVALID_PARAMS,
            "ISSUE_LINK_STALE_IDEA_VERSION",
            "stale Idea row version",
            serde_json::json!({"expected": expected, "actual": actual}),
        ),
        IdeaControlError::IdempotencyConflict => (
            INVALID_PARAMS,
            "ISSUE_LINK_IDEMPOTENCY_CONFLICT",
            "Issue link idempotency conflict",
            serde_json::Value::Null,
        ),
        IdeaControlError::Contention => (
            INTERNAL_ERROR,
            "ISSUE_LINK_CONTENTION",
            "Issue link storage contention",
            serde_json::json!({"retryable": true}),
        ),
        IdeaControlError::ConstraintViolation { class } => (
            INTERNAL_ERROR,
            "ISSUE_LINK_CONSTRAINT_FAILURE",
            "Issue link integrity constraint failed",
            serde_json::json!({"class": class.as_str()}),
        ),
        IdeaControlError::CorruptStoredIssueLink => (
            INTERNAL_ERROR,
            "ISSUE_LINK_CORRUPT_STATE",
            "Issue link stored state is corrupt",
            serde_json::Value::Null,
        ),
        IdeaControlError::StorageFailure(_) => (
            INTERNAL_ERROR,
            "ISSUE_LINK_STORAGE_FAILURE",
            "Issue link storage failure",
            serde_json::Value::Null,
        ),
        _ => (
            INTERNAL_ERROR,
            "ISSUE_LINK_STORAGE_FAILURE",
            "Issue link storage failure",
            serde_json::Value::Null,
        ),
    };
    DaemonError::StructuredRpc {
        rpc_code,
        message: message.to_string(),
        data: serde_json::json!({"code": code, "details": details}),
    }
}

impl RpcServer {
    pub(super) async fn handle_get_issue_tracker_status(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let Some(ref manager) = self.issue_tracker_manager else {
            return Ok(serde_json::json!({
                "enabled": false,
                "tracker": "none",
                "dispatched_count": 0,
                "max_concurrent": 0,
                "poll_interval_ms": 0,
                "active_states": [],
            }));
        };
        let status = manager.status().await;
        Ok(serde_json::to_value(&status)?)
    }

    pub(super) async fn handle_list_dispatched_issues(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let Some(ref manager) = self.issue_tracker_manager else {
            return Ok(serde_json::json!([]));
        };
        let dispatched = manager.dispatched_issues().await;
        Ok(serde_json::to_value(&dispatched)?)
    }

    pub(super) async fn handle_trigger_issue_tracker_poll(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let Some(ref manager) = self.issue_tracker_manager else {
            return Err(DaemonError::Process(
                "Issue tracker not configured".to_string(),
            ));
        };
        let result = manager.trigger_poll().await?;
        Ok(serde_json::to_value(&result)?)
    }

    /// CreateIssue — operator-created issue; `created_by_session_id` is
    /// always `None` here (agent-attributed creation is C5, never this
    /// verb — P-001/P-003).
    pub(super) async fn handle_create_issue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateIssueParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let new = rsi_common::types::NewIssue {
            project_id: params.project_id,
            title: params.title,
            body: params.body,
            priority: params.priority,
            labels: params.labels,
            created_by_session_id: None,
            assignee: params.assignee,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let store = self.session_manager.store().lock().await;
        let issue = store.create_issue(&new)?;
        Ok(serde_json::to_value(&issue)?)
    }

    pub(super) async fn handle_create_issue_v2(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateIssueV2RequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .create_issue_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_issues_page(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListIssuesPageRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .list_issue_workspace_page(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_update_issue_v2(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateIssueRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .update_issue_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_update_issue_status_v2(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateIssueStatusV2RequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .update_issue_status_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_issue_dependencies(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListIssueDependenciesRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .list_issue_workspace_dependencies(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_add_issue_dependency(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: IssueDependencyMutationRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .add_issue_dependency_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_remove_issue_dependency(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: IssueDependencyMutationRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .remove_issue_dependency_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_issue_events_v2(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListIssueEventsV2RequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .list_issue_workspace_events(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_archive_issue_v2(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ArchiveIssueRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .archive_issue_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_restore_issue_v2(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RestoreIssueRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .restore_issue_workspace(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }

    /// GetIssue — unknown id is a not-found error (mirrors `get_label`).
    pub(super) async fn handle_get_issue(&self, request: &RpcRequest) -> Result<serde_json::Value> {
        let params: IssueIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().lock().await;
        let issue = store
            .get_issue(params.issue_id)?
            .ok_or_else(|| DaemonError::Store(format!("Issue not found: {}", params.issue_id)))?;
        Ok(serde_json::to_value(&issue)?)
    }

    pub(super) async fn handle_get_idea(&self, request: &RpcRequest) -> Result<serde_json::Value> {
        let params: GetIdeaParams = serde_json::from_value(request.params.clone())
            .map_err(|error| DaemonError::Rpc(format!("Invalid params: {error}")))?;
        let idea = {
            let store = self.session_manager.store().lock().await;
            store
                .get_idea_with_genesis(params.idea_id)?
                .ok_or_else(|| DaemonError::Store(format!("Idea not found: {}", params.idea_id)))?
        };
        Ok(serde_json::to_value(&idea)?)
    }

    /// Operator-only Issue linkage. Scope comes from the durable Issue row;
    /// the Store rereads it in the semantic transaction to close the TOCTOU.
    pub(super) async fn handle_link_issue_to_idea(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: LinkIssueToIdeaParams = serde_json::from_value(request.params.clone())
            .map_err(|_| {
                link_issue_rpc_error(&IdeaControlError::InvalidRequest("params".into()))
            })?;
        let project_id = {
            let store = self.session_manager.store().lock().await;
            store
                .get_issue(params.issue_id)?
                .ok_or_else(|| link_issue_rpc_error(&IdeaControlError::IssueNotFound))?
                .project_id
        };
        let handle = crate::idea_control::IdeaControlHandle::for_operator(
            self.session_manager.store().clone(),
            project_id,
            "rsi-rpc:LinkIssueToIdea",
        )
        .map_err(|error| link_issue_rpc_error(&error))?;
        let result = handle
            .link_issue_to_idea(&params)
            .await
            .map_err(|error| link_issue_rpc_error(&error))?;
        Ok(serde_json::to_value(result)?)
    }

    /// ListIssues — reuses `IssueFilter` directly as its params type
    /// (already `Default` + `Deserialize`). A missing/null `params` falls
    /// back to the unfiltered default (`RpcRequest.params` defaults to
    /// `Value::Null`, which is load-bearing here), but a present-and-malformed
    /// value errors instead of silently degrading to list-everything
    /// (review NIT-1: `{"status":"Opne"}` must not return all issues).
    pub(super) async fn handle_list_issues(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let filter: rsi_common::types::IssueFilter = if request.params.is_null() {
            rsi_common::types::IssueFilter::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };
        let store = self.session_manager.store().lock().await;
        let issues = store.list_issues(&filter)?;
        Ok(serde_json::to_value(&issues)?)
    }

    /// UpdateIssueStatus — an invalid `status` string fails enum
    /// deserialization (same `DaemonError::Rpc` path as any other param
    /// error); an unknown `issue_id` surfaces the store's own
    /// "Issue not found" error unchanged.
    pub(super) async fn handle_update_issue_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateIssueStatusParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().lock().await;
        let issue = store.update_issue_status(params.issue_id, params.status)?;
        Ok(serde_json::to_value(&issue)?)
    }

    /// Operator-only bounded Issue audit history. It deliberately remains out
    /// of both tokened allowlists; attributed leads use AgentListIssueEvents.
    pub(super) async fn handle_list_issue_events(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListIssueEventsParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let store = self.session_manager.store().lock().await;
        Ok(serde_json::to_value(store.list_issue_events_v1(&params)?)?)
    }

    /// AddIssueDep — self-dep and cycle rejections propagate the store's
    /// own `DaemonError::Store` messages unchanged (P-002); a duplicate
    /// edge is idempotent `Ok(())` (store `INSERT OR IGNORE`), never an
    /// error.
    pub(super) async fn handle_add_issue_dep(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: IssueDepParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().lock().await;
        store.add_issue_dep(params.issue_id, params.depends_on_id)?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// RemoveIssueDep — surfaces the store's `Result<bool>` directly:
    /// `true` if an edge was deleted, `false` if none existed.
    pub(super) async fn handle_remove_issue_dep(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: IssueDepParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let store = self.session_manager.store().lock().await;
        let removed = store.remove_issue_dep(params.issue_id, params.depends_on_id)?;
        Ok(serde_json::json!({ "removed": removed }))
    }

    /// ListReadyIssues — a missing/null `params` falls back to an unbounded
    /// query (`RpcRequest.params` defaults to `Value::Null`; load-bearing),
    /// but a present-and-malformed value errors instead of silently going
    /// unbounded (review NIT-1: `{"limit":"x"}` must not mean "no limit").
    pub(super) async fn handle_list_ready_issues(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListReadyIssuesParams = if request.params.is_null() {
            ListReadyIssuesParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };
        let store = self.session_manager.store().lock().await;
        let issues = store.list_ready_issues(params.project_id, params.limit)?;
        Ok(serde_json::to_value(&issues)?)
    }
}
