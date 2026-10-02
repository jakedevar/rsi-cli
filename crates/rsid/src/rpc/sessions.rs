use super::*;

/// RPC params for getting a specific session (daemon-local, not in common).
#[derive(Debug, Deserialize)]
pub struct GetSessionParams {
    pub session_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueueOperatorMessageParams {
    session_id: Uuid,
    content: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EditOperatorMessageParams {
    message_id: Uuid,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OperatorMessageIdParams {
    message_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InterruptNowParams {
    session_id: Uuid,
    confirmation: String,
}

/// RPC params for interrupting a session (daemon-local, not in common).
#[derive(Debug, Deserialize)]
pub(crate) struct InterruptSessionParams {
    pub(crate) session_id: Uuid,
    #[serde(default = "default_hard_pause")]
    pub(crate) pause_level: crate::store::manager_actions::OperatorPause,
}

pub(super) fn default_hard_pause() -> crate::store::manager_actions::OperatorPause {
    crate::store::manager_actions::OperatorPause::Hard
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetOperatorPauseParams {
    pub(crate) session_id: Uuid,
    pub(crate) pause_level: crate::store::manager_actions::OperatorPause,
}

/// RPC params for getting conversation events (daemon-local, not in common).
#[derive(Debug, Deserialize)]
pub struct GetConversationParams {
    pub session_id: Uuid,
    /// If set, only return events with sequence > since_sequence (incremental fetch).
    #[serde(default)]
    pub since_sequence: Option<i32>,
}

pub(super) const DEFAULT_SESSION_DIAGNOSTIC_PAGE_SIZE: u32 = 50;

/// RPC params for paging session-attributed daemon diagnostics.
#[derive(Debug, Deserialize)]
pub struct GetSessionDiagnosticsParams {
    pub session_id: Uuid,
    #[serde(default)]
    pub after_id: Option<i64>,
    #[serde(default = "default_session_diagnostic_page_size")]
    pub limit: u32,
}

pub(super) fn default_session_diagnostic_page_size() -> u32 {
    DEFAULT_SESSION_DIAGNOSTIC_PAGE_SIZE
}

/// RPC params for deleting a session (daemon-local, not in common).
#[derive(Debug, Deserialize)]
pub struct DeleteSessionParams {
    pub session_id: Uuid,
}

/// RPC params for marking an active session for auto-archive on completion.
#[derive(Debug, Deserialize)]
pub struct MarkPendingArchiveParams {
    pub session_id: Uuid,
    /// Desired pending-archive state. Defaults to `true` for backward
    /// compatibility with callers that only ever set the flag.
    #[serde(default = "default_pending_archive")]
    pub pending: bool,
}

pub(super) fn default_pending_archive() -> bool {
    true
}

/// RPC params for toggling session pin state (daemon-local, not in common).
#[derive(Debug, Deserialize)]
pub struct TogglePinParams {
    pub session_id: Uuid,
}

/// RPC params for toggling session "testing needed" marker.
#[derive(Debug, Deserialize)]
pub struct ToggleTestingNeededParams {
    pub session_id: Uuid,
}

/// RPC params for toggling per-session rotation disable.
#[derive(Debug, Deserialize)]
pub struct ToggleRotationDisabledParams {
    pub session_id: Uuid,
}

/// RPC params for updating a session's project assignment.
#[derive(Debug, Deserialize)]
pub struct UpdateSessionProjectParams {
    pub session_id: Uuid,
    /// None = clear project assignment (unassigned).
    pub project_id: Option<Uuid>,
}

/// RPC params for updating a session's workflow assignment.
#[derive(Debug, Deserialize)]
pub struct UpdateSessionWorkflowParams {
    pub session_id: Uuid,
    /// None = clear workflow assignment.
    pub workflow_id: Option<Uuid>,
}

/// RPC params for updating a session's title.
#[derive(Debug, Deserialize)]
pub struct UpdateSessionTitleParams {
    pub session_id: Uuid,
    pub title: String,
}

/// RPC params for updating a session's description.
#[derive(Debug, Deserialize)]
pub struct UpdateSessionDescriptionParams {
    pub session_id: Uuid,
    pub description: String,
}

/// RPC params for updating a session's active task context.
#[derive(Debug, Deserialize)]
pub struct UpdateActiveTaskParams {
    pub session_id: Uuid,
    pub active_task: Option<String>,
}

/// RPC params for listing archived sessions with optional project filter.
#[derive(Debug, Default, Deserialize)]
pub struct ListArchivedSessionsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// RPC params for unarchiving a session.
#[derive(Debug, Deserialize)]
pub struct UnarchiveSessionParams {
    pub session_id: Uuid,
}

/// RPC params for listing deleted sessions with optional project filter.
#[derive(Debug, Default, Deserialize)]
pub struct ListDeletedSessionsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// RPC params for getting session summaries.
#[derive(Debug, Deserialize)]
pub struct GetSessionSummaryParams {
    pub session_id: Uuid,
    /// Optional filter: "Short" or "Long". If omitted, returns both latest.
    #[serde(default)]
    pub kind: Option<String>,
}

/// RPC params for undeleting (restoring from trash) a session.
#[derive(Debug, Deserialize)]
pub struct UndeleteSessionParams {
    pub session_id: Uuid,
}

/// RPC params for permanently purging a session from trash.
#[derive(Debug, Deserialize)]
pub struct PurgeSessionParams {
    pub session_id: Uuid,
}

/// RPC params for ListSessions with optional project filter.
#[derive(Debug, Default, Deserialize)]
pub struct ListSessionsParams {
    /// Filter to a specific project. Omit for all sessions.
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// RPC params for updating a session's label assignment.
#[derive(Debug, Deserialize)]
pub struct UpdateSessionLabelParams {
    pub session_id: Uuid,
    pub group_id: Option<Uuid>,
}

/// RPC params for memory search.
///
/// `project_id` is optional and defaults to `None` for backward compatibility.
/// When set, the daemon restricts search to chunks indexed under that project.
/// When unset, the search is global (manual / admin / debug behavior).
#[derive(Debug, Deserialize)]
pub struct MemorySearchParams {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub min_score: Option<f64>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// RPC params for memory index (re-sync).
#[derive(Debug, Default, Deserialize)]
pub struct MemoryIndexParams {
    #[serde(default)]
    pub force: bool,
}

/// RPC params for reading a memory file.
#[derive(Debug, Deserialize)]
pub struct MemoryReadParams {
    pub path: String,
    /// 1-indexed start line.
    #[serde(default)]
    pub from: Option<usize>,
    /// Number of lines to read.
    #[serde(default)]
    pub lines: Option<usize>,
}

/// RPC params for listing observations.
#[derive(Debug, Default, Deserialize)]
pub struct ListObservationsParams {
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// RPC params for searching observations.
///
/// `project_id` is optional and defaults to `None`. When set, observations are
/// restricted to the matching project.
#[derive(Debug, Deserialize)]
pub struct SearchObservationsParams {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

impl RpcServer {
    pub(super) async fn handle_answer_question(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::rpc::AnswerQuestionParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .answer_question(params.session_id, params.response_text)
            .await?;

        Ok(serde_json::json!({ "success": true }))
    }

    pub(super) async fn handle_launch_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        // params defaults to Null via serde - no unwrap_or needed
        let params: LaunchSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::InvalidParam(format!("Invalid params: {}", e)))?;

        // Validate and canonicalize working_dir before any background work.
        // This surfaces path errors as RPC responses (TUI notification) rather than
        // silent session failures.
        let requested_working_dir = params.working_dir.clone();
        let resolved_working_dir = requested_working_dir
            .as_deref()
            .map(crate::path_safety::canonicalize_working_dir)
            .transpose()?;
        let resolved_working_dir = match (resolved_working_dir, params.project_id) {
            (Some(working_dir), _) => Some(working_dir),
            (None, Some(project_id)) => Some(
                self.session_manager
                    .resolve_project_working_dir(project_id)
                    .await?,
            ),
            (None, None) => None,
        };

        // Optional workspace root containment check.
        if let Some(ref canon_dir) = resolved_working_dir {
            let roots = self.session_manager.workspace_roots();
            crate::path_safety::validate_containment(canon_dir, roots)?;
        }

        // Validate parent containment if parent_id is supplied.
        if let Some(parent_id) = params.parent_id {
            let parent = {
                // Try memory-resident sessions first; fall back to DB for
                // containers/archived rows that are not in active/completed maps.
                if let Some(s) = self.session_manager.get_session(parent_id).await {
                    s
                } else {
                    let store = self.session_manager.store().clone();
                    tokio::task::spawn_blocking(move || {
                        let store = store.blocking_lock();
                        store.get_session(parent_id)
                    })
                    .await
                    .map_err(|e| DaemonError::Store(e.to_string()))??
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!("parent not found: {parent_id}"))
                    })?
                }
            };
            let child_kind = params.session_kind.unwrap_or_default();
            crate::session::hierarchy::validate_containment(Some(parent.session_kind), child_kind)
                .map_err(|e| DaemonError::InvalidParam(format!("illegal_parent: {e}")))?;
            // Note: cycle-check is not needed at launch — the new session has no
            // preexisting children, so no ancestor walk can encounter it.
        }
        // Resolve the declared capability class from the leading `/<command>`
        // in `query` via the command-frontmatter registry (RSI-010). Computed
        // before the move into LaunchConfig.
        let resolved_class = self
            .session_manager
            .command_registry()
            .class_for_query(&params.query);

        // ─── P1.6: validate and normalize tags ──────────────────────────────────
        let validated_tags: Vec<String> = {
            let raw = &params.tags;
            if raw.is_empty() {
                return Err(DaemonError::InvalidParam(
                    "tags required: at least one tag must be provided".to_string(),
                ));
            }
            let mut out = Vec::with_capacity(raw.len());
            for t in raw {
                match rsi_common::normalize_tag(t) {
                    Ok(normalized) => out.push(normalized),
                    Err(_) => {
                        return Err(DaemonError::InvalidParam(format!("tag_malformed: {t}")));
                    }
                }
            }
            out.sort();
            out.dedup();
            out
        };

        // #792: the operator-only Harness tool policy is validated before any
        // launch effect, and only for providers that run the Harness loop
        // (the only place it can be enforced).
        if let Some(policy) = &params.tool_policy {
            policy
                .validate()
                .map_err(|code| DaemonError::InvalidParam(code.to_string()))?;
            if !params
                .provider
                .is_some_and(rsi_common::harness_tool_policy::provider_runs_harness_loop)
            {
                return Err(DaemonError::InvalidParam(
                    rsi_common::harness_tool_policy::TOOL_POLICY_UNSUPPORTED_PROVIDER.to_string(),
                ));
            }
        }
        if let Some(gates) = &params.completion_gates {
            gates
                .validate()
                .map_err(|code| DaemonError::InvalidParam(code.to_string()))?;
            if !params
                .provider
                .is_some_and(rsi_common::harness_tool_policy::provider_runs_harness_loop)
            {
                return Err(DaemonError::InvalidParam(
                    rsi_common::completion_gates::COMPLETION_GATES_UNSUPPORTED_PROVIDER.to_string(),
                ));
            }
        }

        // Map LaunchSessionParams to LaunchConfig
        let config = LaunchConfig {
            completion_gates: params.completion_gates.clone(),
            query: params.query,
            title: params.title,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: resolved_working_dir,
            provider: params.provider,
            model: params.model,
            configured_context_window: params.configured_context_window,
            max_turns: None,
            system_prompt: params.system_prompt,
            session_kind: params.session_kind,
            resume_session_id: None,
            project_id: params.project_id,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: params.continued_from,
            openai_base_url: params.openai_base_url,
            openai_api_key: params.openai_api_key,
            conversation_history: None,
            workflow_id: params.workflow_id,
            workflow_id_override: params.workflow_id_override,
            max_retries: params.max_retries,
            group_id: params.group_id,
            parent_id: params.parent_id,
            effort: params.effort,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            skip_project_model_default: false,
            tool_policy: params.tool_policy,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            sandbox: params.sandbox,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: params.is_eval.unwrap_or(false),
            skip_context_pipeline: params.skip_context_pipeline.unwrap_or(false),
            // Resolved above via the command-frontmatter registry (RSI-010).
            capability_class: resolved_class,
            tags: validated_tags.clone(),
            // P1.7: RPC-direct launches are not topology-bound; coordinator
            // path handles binding for spawn-directive children.
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        };

        let session_id = self.session_manager.launch_session(config).await?;

        // ─── P1.6: persist validated tags to session_tags join table ────────────
        if !validated_tags.is_empty() {
            if let Err(e) = self
                .session_manager
                .update_session_tags(session_id, validated_tags)
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "P1.6: failed to persist tags for launched session"
                );
            }
        }

        Ok(serde_json::json!({
            "session_id": session_id
        }))
    }

    pub(super) async fn handle_get_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.require_attributed_read(request, vec![params.session_id], AgentReadClass::Metadata)
            .await?;

        let session = self
            .session_manager
            .get_session(params.session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(params.session_id))?;

        Ok(serde_json::to_value(&session)?)
    }

    pub(super) async fn handle_list_sessions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        // Parse optional params (defaults to empty = all sessions)
        let params: ListSessionsParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();

        let sessions = if let Some(project_id) = params.project_id {
            self.session_manager
                .list_sessions_by_project(Some(project_id))
                .await
        } else {
            self.session_manager.list_sessions().await
        };
        Ok(serde_json::to_value(&sessions)?)
    }

    pub(super) async fn handle_get_conversation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetConversationParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.require_attributed_read(request, vec![params.session_id], AgentReadClass::Content)
            .await?;

        let events = self
            .session_manager
            .get_conversation_since(params.session_id, params.since_sequence)
            .await?;
        Ok(serde_json::to_value(&events)?)
    }

    pub(super) async fn handle_get_session_diagnostics(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetSessionDiagnosticsParams = serde_json::from_value(request.params.clone())
            .map_err(|error| DaemonError::Rpc(format!("Invalid params: {error}")))?;
        self.require_attributed_read(request, vec![params.session_id], AgentReadClass::Content)
            .await?;

        let diagnostics = self
            .session_manager
            .get_session_diagnostics(params.session_id, params.after_id, params.limit)
            .await?;
        let next_after_id = (diagnostics.len() == params.limit as usize)
            .then(|| diagnostics.last().map(|diagnostic| diagnostic.id))
            .flatten();
        Ok(serde_json::to_value(
            rsi_common::types::SessionDiagnosticsPageV1 {
                diagnostics,
                next_after_id,
            },
        )?)
    }

    pub(super) async fn handle_get_conversations_since(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetConversationsSinceParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        // A batch is all-or-nothing for attributed callers: one unreadable
        // cursor refuses the request rather than silently dropping a session
        // the caller would misread as having no new events.
        self.require_attributed_read(
            request,
            params
                .requests
                .iter()
                .map(|cursor| cursor.session_id)
                .collect(),
            AgentReadClass::Content,
        )
        .await?;

        let mut conversations = Vec::with_capacity(params.requests.len());
        for cursor in params.requests {
            let events = self
                .session_manager
                .get_conversation_since(cursor.session_id, cursor.since_sequence)
                .await?;
            conversations.push(ConversationBatchEntry {
                session_id: cursor.session_id,
                events,
            });
        }

        Ok(serde_json::to_value(ConversationBatchResponse {
            conversations,
        })?)
    }

    pub(super) async fn handle_get_turn_metrics(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        // Reuse GetConversationParams - same shape (just session_id)
        let params: GetConversationParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.require_attributed_read(request, vec![params.session_id], AgentReadClass::Content)
            .await?;

        let metrics = self
            .session_manager
            .get_turn_metrics(params.session_id)
            .await?;
        Ok(serde_json::to_value(&metrics)?)
    }

    pub(super) async fn handle_interrupt_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: InterruptSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        if params.pause_level == crate::store::manager_actions::OperatorPause::None {
            return Err(DaemonError::InvalidParam(
                "interrupt_pause_level_required".into(),
            ));
        }
        self.session_manager
            .interrupt_session_operator_with_pause(params.session_id, params.pause_level)
            .await?;

        Ok(serde_json::Value::Null)
    }

    pub(super) async fn handle_set_operator_pause(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: SetOperatorPauseParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let level = self
            .session_manager
            .store()
            .lock()
            .await
            .set_operator_pause(params.session_id, params.pause_level)?;
        Ok(serde_json::json!({"session_id": params.session_id, "pause_level": level}))
    }

    pub(super) async fn handle_get_operator_pause(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let level = self
            .session_manager
            .store()
            .lock()
            .await
            .get_operator_pause(params.session_id)?;
        Ok(serde_json::json!({"session_id": params.session_id, "pause_level": level}))
    }

    pub(super) async fn handle_cancel_retry(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: InterruptSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let cancelled = self.session_manager.cancel_retry(params.session_id).await?;

        Ok(serde_json::json!({ "cancelled": cancelled }))
    }

    pub(super) async fn handle_rotate_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RotateSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .trigger_rotation(params.session_id)
            .await?;

        Ok(serde_json::Value::Null)
    }

    pub(super) async fn handle_delete_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: DeleteSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .delete_session(params.session_id)
            .await?;

        Ok(serde_json::Value::Null)
    }

    pub(super) async fn handle_mark_pending_archive(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: MarkPendingArchiveParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .mark_pending_archive(params.session_id, params.pending)
            .await?;

        Ok(serde_json::Value::Null)
    }

    /// Archive clears any Epic lead pointer that references the archived
    /// session. Lead validity requires a direct child leaf that is neither
    /// archived nor deleted.
    pub(super) async fn handle_archive_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ArchiveSessionParamsV1 = serde_json::from_value(request.params.clone())
            .map_err(|_| DaemonError::InvalidParam("invalid ArchiveSession request".into()))?;

        let result = self
            .session_manager
            .archive_session(params.session_id)
            .await?;

        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_archived_sessions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListArchivedSessionsParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();

        let sessions = self
            .session_manager
            .list_archived_sessions(params.project_id)
            .await?;

        Ok(serde_json::to_value(&sessions)?)
    }

    pub(super) async fn handle_unarchive_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UnarchiveSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let session = self
            .session_manager
            .unarchive_session(params.session_id)
            .await?;

        Ok(serde_json::to_value(&session)?)
    }

    pub(super) async fn handle_list_deleted_sessions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListDeletedSessionsParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();

        let sessions = self
            .session_manager
            .list_deleted_sessions(params.project_id)
            .await?;

        Ok(serde_json::to_value(&sessions)?)
    }

    pub(super) async fn handle_undelete_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UndeleteSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let session = self
            .session_manager
            .undelete_session(params.session_id)
            .await?;

        Ok(serde_json::to_value(&session)?)
    }

    pub(super) async fn handle_purge_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: PurgeSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .purge_session(params.session_id)
            .await?;

        Ok(serde_json::Value::Null)
    }

    pub(super) async fn handle_toggle_pin(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: TogglePinParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let pinned_at = self.session_manager.toggle_pin(params.session_id).await?;
        Ok(serde_json::json!({ "pinned_at": pinned_at }))
    }

    pub(super) async fn handle_toggle_testing_needed(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ToggleTestingNeededParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let testing_needed_at = self
            .session_manager
            .toggle_testing_needed(params.session_id)
            .await?;
        Ok(serde_json::json!({ "testing_needed_at": testing_needed_at }))
    }

    pub(super) async fn handle_toggle_rotation_disabled(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ToggleRotationDisabledParams =
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let rotation_disabled_at = self
            .session_manager
            .toggle_rotation_disabled(params.session_id)
            .await?;
        Ok(serde_json::json!({ "rotation_disabled_at": rotation_disabled_at }))
    }

    pub(super) async fn handle_update_session_project(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionProjectParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager
            .update_session_project(params.session_id, params.project_id)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_update_session_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionWorkflowParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager
            .update_session_workflow(params.session_id, params.workflow_id)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_update_session_title(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionTitleParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager
            .update_session_title(params.session_id, params.title)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_update_session_description(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionDescriptionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager
            .update_session_description(params.session_id, params.description)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_update_active_task(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateActiveTaskParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager
            .update_active_task(params.session_id, params.active_task)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_update_session_rating(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionRatingParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        if let Some(v) = params.rating {
            if !(1..=10).contains(&v) {
                return Err(DaemonError::Rpc(
                    "Rating must be between 1 and 10".to_string(),
                ));
            }
        }
        self.session_manager
            .update_session_rating(params.session_id, params.rating)
            .await?;
        Ok(serde_json::json!({
            "session_id": params.session_id,
            "rating": params.rating,
        }))
    }

    pub(super) async fn handle_queue_session_model_update(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: QueueSessionModelUpdateParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let receipt = self
            .session_manager
            .store()
            .lock()
            .await
            .queue_session_model_update(
                params.session_id,
                params.expected_model_invocation_id,
                &params.new_model,
                params.new_effort.as_deref(),
                &params.idempotency_key,
            )?;
        Ok(serde_json::to_value(receipt)?)
    }

    pub(super) async fn handle_continue_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ContinueSessionParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        // #1049: an operator message to a running Claude turn is queued for the
        // next tool boundary; it never interrupts the in-flight tool call.
        if self
            .session_manager
            .queue_operator_message_if_turn_active(params.session_id, &params.query)
            .await?
        {
            return Ok(serde_json::Value::Null);
        }
        self.session_manager
            .continue_session_operator(params.session_id, params.query)
            .await?;

        Ok(serde_json::Value::Null)
    }

    pub(super) async fn handle_queue_operator_message(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: QueueOperatorMessageParams = serde_json::from_value(request.params.clone())?;
        let message = self
            .session_manager
            .store()
            .lock()
            .await
            .queue_operator_message(params.session_id, &params.content, &params.idempotency_key)?;
        Ok(serde_json::to_value(message)?)
    }

    pub(super) async fn handle_list_operator_messages(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetSessionParams = serde_json::from_value(request.params.clone())?;
        let messages = self
            .session_manager
            .store()
            .lock()
            .await
            .list_operator_messages(params.session_id)?;
        Ok(serde_json::to_value(messages)?)
    }

    pub(super) async fn handle_edit_operator_message(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: EditOperatorMessageParams = serde_json::from_value(request.params.clone())?;
        let message = self
            .session_manager
            .store()
            .lock()
            .await
            .edit_operator_message(params.message_id, &params.content)?;
        Ok(serde_json::to_value(message)?)
    }

    pub(super) async fn handle_withdraw_operator_message(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: OperatorMessageIdParams = serde_json::from_value(request.params.clone())?;
        let message = self
            .session_manager
            .store()
            .lock()
            .await
            .withdraw_operator_message(params.message_id)?;
        Ok(serde_json::to_value(message)?)
    }

    pub(super) async fn handle_interrupt_session_now(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: InterruptNowParams = serde_json::from_value(request.params.clone())?;
        if params.confirmation != "INTERRUPT NOW" {
            return Err(DaemonError::InvalidParam(
                "type INTERRUPT NOW to confirm cancellation".into(),
            ));
        }
        self.session_manager
            .interrupt_session_operator_with_pause(
                params.session_id,
                crate::store::manager_actions::OperatorPause::Hard,
            )
            .await?;
        Ok(serde_json::json!({"cancelled": true}))
    }

    pub(super) async fn handle_update_session_label(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionLabelParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager
            .update_session_label(params.session_id, params.group_id)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// CreateContainer — materialize a Group/Epic organizational node.
    ///
    /// Validates `legal_children(parent_kind)` covers `params.kind`, then
    /// builds a Session row directly (status = Completed, no provider
    /// subprocess) and persists it via the existing background-write
    /// pipeline. Never routes through `SessionManager::launch_session` —
    /// the spawn gate would reject container kinds anyway.
    pub(super) async fn handle_create_container(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateContainerParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        // ─── P1.6: validate topology_id (Epic-only + row exists) ────────────────
        if let Some(topology_id) = params.topology_id {
            if params.kind != rsi_common::types::SessionKind::Epic {
                return Err(DaemonError::InvalidParam(format!(
                    "illegal_topology_id_on_non_epic: topology_id is only valid when kind == Epic, got {:?}",
                    params.kind
                )));
            }
            // Verify topology row exists
            let store = self.session_manager.store().clone();
            let exists = tokio::task::spawn_blocking(move || {
                let guard = store.blocking_lock();
                guard.topology_exists(topology_id)
            })
            .await
            .map_err(|e| DaemonError::Store(e.to_string()))??;
            if !exists {
                return Err(DaemonError::InvalidParam(format!(
                    "topology not found: {topology_id}"
                )));
            }
        }

        let session = self.session_manager.create_container(params).await?;
        Ok(serde_json::json!({ "id": session.id }))
    }

    /// SetSessionParent — reparent an existing session in the hierarchy.
    ///
    /// Validates containment legality and cycle-freeness via the pure-fn
    /// validators, then issues a single `update_session_parent` write.
    pub(super) async fn handle_set_session_parent(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: SetSessionParentParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .set_session_parent(params.session_id, params.new_parent_id)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// ListSessionChildren — return direct children of a hierarchy parent.
    /// `parent_id = None` returns top-level sessions (rows with `parent_id IS NULL`).
    pub(super) async fn handle_list_session_children(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListSessionChildrenParams = serde_json::from_value(request.params.clone())
            .unwrap_or(ListSessionChildrenParams { parent_id: None });

        let reader = self.attributed_read_caller(request).await?;
        let store = self.session_manager.store().clone();
        let parent_id = params.parent_id;
        let mut children = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let Some(caller) = reader else {
                return store.list_children(parent_id);
            };
            // #241: an attributed caller must be able to read the named
            // anchor itself, and sees only the children inside its read
            // scope (a root listing likewise shows only readable roots).
            let scope = store.agent_read_scope(caller)?;
            if let Some(anchor) = parent_id
                && !store.agent_read_scope_admits(&scope, anchor, AgentReadClass::Metadata)?
            {
                return Err(agent_read_scope_denied());
            }
            let children = store.list_children(parent_id)?;
            let mut readable = Vec::with_capacity(children.len());
            for child in children {
                if store.agent_read_scope_admits(&scope, child.id, AgentReadClass::Metadata)? {
                    readable.push(child);
                }
            }
            Ok(readable)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        self.session_manager
            .stamp_context_fill_pct(&mut children)
            .await;
        Ok(serde_json::to_value(&children)?)
    }

    /// SetEpicLead — set or clear the lead-session pointer on an Epic container.
    ///
    /// Validation (when `new_lead_session_id` is `Some`):
    /// - `epic_id` must resolve to an Epic-kind session.
    /// - The candidate lead must be a leaf kind.
    /// - The candidate lead must be a direct child of the Epic (`parent_id == epic_id`).
    /// - The candidate lead must not be Archived or Deleted.
    ///
    /// Passing `new_lead_session_id: None` unconditionally clears the pointer.
    pub(super) async fn handle_set_epic_lead(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: SetEpicLeadParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .set_epic_lead(params.epic_id, params.new_lead_session_id)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// UpdateSessionTags — replace the full tag set for a session (atomic).
    pub(super) async fn handle_update_session_tags(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateSessionTagsParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .update_session_tags(params.session_id, params.tags)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// AddSessionTag — add a single tag to a session (idempotent).
    pub(super) async fn handle_add_session_tag(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: AddSessionTagParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .add_session_tag(params.session_id, params.tag)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// RemoveSessionTag — remove a single tag from a session (idempotent).
    pub(super) async fn handle_remove_session_tag(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RemoveSessionTagParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager
            .remove_session_tag(params.session_id, params.tag)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// ListTags — return tags with session-counts, optionally filtered by
    /// prefix and/or project_id. Capped at 100 results.
    pub(super) async fn handle_list_tags(&self, request: &RpcRequest) -> Result<serde_json::Value> {
        let params: ListTagsParams =
            serde_json::from_value(request.params.clone()).unwrap_or(ListTagsParams {
                prefix: None,
                project_id: None,
            });

        let tags = self
            .session_manager
            .list_tags(params.prefix, params.project_id)
            .await?;
        Ok(serde_json::to_value(&tags)?)
    }

    pub(super) async fn handle_get_session_summary(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetSessionSummaryParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.require_attributed_read(request, vec![params.session_id], AgentReadClass::Metadata)
            .await?;

        let store = self.session_manager.store().clone();
        let session_id = params.session_id;
        let kind_filter = params.kind;

        let result = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            match kind_filter.as_deref() {
                Some("Short") => {
                    let summary = store
                        .get_latest_summary(session_id, rsi_common::types::SummaryKind::Short)?;
                    Ok(serde_json::to_value(&summary)?)
                }
                Some("Long") => {
                    let summary = store
                        .get_latest_summary(session_id, rsi_common::types::SummaryKind::Long)?;
                    Ok(serde_json::to_value(&summary)?)
                }
                Some(other) => Err(DaemonError::Rpc(format!(
                    "Invalid summary kind: '{}' (expected 'Short' or 'Long')",
                    other
                ))),
                None => {
                    let (short, long) = store.get_latest_summaries(session_id)?;
                    Ok(serde_json::json!({
                        "short": short,
                        "long": long,
                    }))
                }
            }
        })
        .await
        .map_err(|e| DaemonError::Rpc(format!("Store task failed: {}", e)))??;

        Ok(result)
    }
}
