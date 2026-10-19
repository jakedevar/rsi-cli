use super::*;

impl RpcServer {
    /// Resolve a copied caller incarnation; dispatch scopes the original
    /// resolution across every await without retaining a registry guard.
    pub(super) async fn resolve_bound_caller(
        &self,
        request: &RpcRequest,
    ) -> Result<crate::session::BoundAgentCaller> {
        if let Some(admission) = crate::session::rpc_admission::current() {
            return Ok(admission.caller);
        }
        let token = request
            .session_token
            .as_ref()
            .ok_or_else(|| DaemonError::InvalidParam("agent_verb_requires_session_token".into()))?;
        self.session_manager
            .resolve_bound_agent_caller(token.expose_for_transport())
            .await
            .ok_or_else(crate::session::rpc_admission::unknown_caller)
    }

    /// Compatibility for fresh launches only. An unfenced handler cannot
    /// silently turn an adopted activation into bare-UUID authority (N3).
    pub(super) async fn resolve_caller_session_id(&self, request: &RpcRequest) -> Result<Uuid> {
        self.resolve_bound_caller(request)
            .await?
            .legacy_session_id()
            .ok_or_else(crate::session::rpc_admission::unknown_caller)
    }

    /// The token-resolved caller of a read verb, or `None` for an
    /// unattributed (operator/TUI) call, which read scoping never touches.
    pub(super) async fn attributed_read_caller(
        &self,
        request: &RpcRequest,
    ) -> Result<Option<Uuid>> {
        if request.session_token.is_none() {
            return Ok(None);
        }
        self.resolve_caller_session_id(request).await.map(Some)
    }

    /// #241 target scoping for session-attributed read verbs: every named
    /// target must be inside the token caller's read scope
    /// (`Store::agent_read_scope_admits`), otherwise the whole request is
    /// refused with `agent_read_scope_denied` before any target is read.
    /// Unattributed calls return immediately, unchanged.
    pub(super) async fn require_attributed_read(
        &self,
        request: &RpcRequest,
        targets: Vec<Uuid>,
        class: AgentReadClass,
    ) -> Result<()> {
        let Some(caller) = self.attributed_read_caller(request).await? else {
            return Ok(());
        };
        let store = self.session_manager.store().lock().await;
        let scope = store.agent_read_scope(caller)?;
        for target in targets {
            if !store.agent_read_scope_admits(&scope, target, class)? {
                return Err(agent_read_scope_denied());
            }
        }
        // Scope/data preparation is private and Store still owns the scope.
        // A6 is acquired last for the bounded snapshot admission itself.
        let _admission = crate::session::rpc_admission::admit_current_rpc(caller).await?;
        Ok(())
    }

    pub(super) async fn handle_agent_spawn_child(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentSpawnChildRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        params
            .validate()
            .map_err(|e| DaemonError::InvalidParam(e.to_string()))?;

        let outcome = self
            .session_manager
            .agent_spawn_child(caller_session_id, params)
            .await;

        match outcome {
            crate::session::agent_verbs::AgentSpawnChildOutcome::Accepted(result) => {
                Ok(serde_json::to_value(result)?)
            }
            crate::session::agent_verbs::AgentSpawnChildOutcome::Rejected(reason) => Err(
                DaemonError::InvalidParam(format!("agent_spawn_rejected:{reason:?}")),
            ),
        }
    }

    pub(super) async fn handle_agent_reserve_successor(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentReserveSuccessorRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        params
            .validate()
            .map_err(|e| DaemonError::InvalidParam(e.to_string()))?;
        let result = self
            .session_manager
            .agent_reserve_successor(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_get_progress(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentGetProgressParamsV1 = if request.params.is_null() {
            AgentGetProgressParamsV1::default()
        } else {
            // #2312: session ids may be unique prefixes inside the caller's scope.
            let expanded = self
                .session_manager
                .agent_control()
                .expand_id_prefixes(
                    caller_session_id,
                    AgentControlVerbV1::GetProgress,
                    request.params.clone(),
                )
                .await?;
            serde_json::from_value(expanded)
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };
        let result = self
            .session_manager
            .agent_get_progress(caller_session_id, &params.session_ids)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    /// `AgentSendMessage` — durable owner→child mail (P2-03).
    ///
    /// The request struct carries `target_session_id`, the bounded message, an
    /// idempotency key, and an optional expiry — and NOTHING else. Sender
    /// identity and Epic scope are resolved from the transport token here, so
    /// a caller-supplied sender field cannot exist to be honoured: the strict
    /// `AgentSendMessageRequestV1` is `deny_unknown_fields`, so an attempt to
    /// smuggle one is a deserialization error before any authority check.
    pub(super) async fn handle_agent_send_message(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentSendMessageRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let receipt = self
            .session_manager
            .agent_send_message(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// `AgentGetAuthorityCatalog`: the token-bound caller's operator's manual.
    pub(super) async fn handle_agent_get_authority_catalog(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::agent_authority_catalog::{
            AUTHORITY_CATALOG_INVALID_REQUEST, AgentGetAuthorityCatalogRequestV1,
        };
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentGetAuthorityCatalogRequestV1 = if request.params.is_null() {
            AgentGetAuthorityCatalogRequestV1::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam(AUTHORITY_CATALOG_INVALID_REQUEST.into()))?
        };
        let catalog = self
            .session_manager
            .agent_control()
            .agent_get_authority_catalog(caller, params)
            .await?;
        Ok(serde_json::to_value(catalog)?)
    }

    /// #1049: the session's own tool-boundary hook claims its pending mail.
    /// The target is ALWAYS the token-resolved caller; params are ignored.
    pub(super) async fn handle_claim_boundary_mail(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let claimed = self
            .session_manager
            .claim_boundary_mail(caller_session_id)
            .await?;
        Ok(serde_json::to_value(claimed)?)
    }

    /// #1183: the session's own hook confirms it wrote the mail it claimed.
    /// Only ids this caller's hook claimed are confirmed; params carry the ids
    /// and nothing else.
    pub(super) async fn handle_confirm_boundary_mail(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::boundary_mail_hook::ConfirmBoundaryMailParams =
            serde_json::from_value(request.params.clone()).map_err(|_| {
                DaemonError::InvalidParam("confirm_boundary_mail_invalid_request".into())
            })?;
        let confirmed = self
            .session_manager
            .confirm_boundary_mail(caller_session_id, &params)
            .await?;
        Ok(serde_json::to_value(confirmed)?)
    }

    /// #2766: the session's own lifecycle hook reports `StopFailure`,
    /// `PreCompact` or (hub #2919) `SessionStart`. Params carry the event and a
    /// bounded summary only.
    pub(super) async fn handle_record_provider_lifecycle_event(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::provider_lifecycle_hooks::RecordProviderLifecycleEventParams =
            serde_json::from_value(request.params.clone()).map_err(|_| {
                DaemonError::InvalidParam("provider_lifecycle_event_invalid_request".into())
            })?;
        let recorded = self
            .session_manager
            .record_provider_lifecycle_event(caller_session_id, params)
            .await;
        Ok(serde_json::to_value(recorded)?)
    }

    /// `AgentSubmitJob` (#1002): hand a typed long operation to the daemon.
    pub(super) async fn handle_agent_submit_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::agent_jobs::AgentSubmitJobRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("job_invalid_request".into()))?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_submit_job(
                caller,
                params,
                crate::agent_jobs::platform_job_runtime(),
                crate::agent_jobs::JobTools::discover(),
            )
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    pub(super) async fn handle_agent_get_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        // #2312: the job id may be the unique prefix a notice prints.
        let expanded = self
            .session_manager
            .agent_control()
            .expand_id_prefixes(caller, AgentControlVerbV1::GetJob, request.params.clone())
            .await?;
        let params: rsi_common::agent_jobs::AgentGetJobRequestV1 =
            rsi_common::harness_manager::decode_manager_request(expanded)
                .map_err(|_| DaemonError::InvalidParam("job_invalid_request".into()))?;
        let job = self
            .session_manager
            .agent_control()
            .agent_get_job(caller, params)
            .await?;
        Ok(serde_json::to_value(job)?)
    }

    /// `AgentCancelJob` (#1106): stop one of the caller's running jobs.
    pub(super) async fn handle_agent_cancel_job(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::agent_jobs::AgentCancelJobRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("job_invalid_request".into()))?;
        let cancelled = self
            .session_manager
            .agent_control()
            .agent_cancel_job(caller, params, crate::agent_jobs::platform_job_runtime())
            .await?;
        Ok(serde_json::to_value(cancelled)?)
    }

    pub(super) async fn handle_agent_list_jobs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::agent_jobs::AgentListJobsRequestV1 = if request.params.is_null() {
            rsi_common::agent_jobs::AgentListJobsRequestV1 { limit: None }
        } else {
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("job_invalid_request".into()))?
        };
        let listed = self
            .session_manager
            .agent_control()
            .agent_list_jobs(caller, params)
            .await?;
        Ok(serde_json::to_value(listed)?)
    }

    pub(super) async fn handle_agent_enqueue_landing_source(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::rolling_queue::AgentEnqueueLandingSourceRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("queue_invalid_request".into()))?;
        let enabled = self
            .runtime_config
            .rolling_queue_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let receipt = self
            .session_manager
            .agent_control()
            .agent_enqueue_landing_source(caller, params, enabled)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// `AgentListRollingQueue` (#1964).
    pub(super) async fn handle_agent_list_rolling_queue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::rolling_queue::AgentListRollingQueueRequestV1 =
            if request.params.is_null() {
                Default::default()
            } else {
                rsi_common::harness_manager::decode_manager_request(request.params.clone())
                    .map_err(|_| {
                        DaemonError::InvalidParam(
                            rsi_common::rolling_queue::QUEUE_INVALID_REQUEST.into(),
                        )
                    })?
            };
        let receipt = self
            .session_manager
            .agent_control()
            .agent_list_rolling_queue(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// `AgentCancelRollingQueueEntry` (#1964).
    pub(super) async fn handle_agent_cancel_rolling_queue_entry(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::rolling_queue::AgentCancelRollingQueueEntryRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone()).map_err(
                |_| {
                    DaemonError::InvalidParam(
                        rsi_common::rolling_queue::QUEUE_INVALID_REQUEST.into(),
                    )
                },
            )?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_cancel_rolling_queue_entry(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// `AgentCheckLandingCandidate` (#2310).
    pub(super) async fn handle_agent_check_landing_candidate(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::agent_landing_candidate::AgentCheckLandingCandidateRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone()).map_err(
                |_| {
                    DaemonError::InvalidParam(
                        rsi_common::agent_landing_candidate::CANDIDATE_INVALID_REQUEST.into(),
                    )
                },
            )?;
        let result = self
            .session_manager
            .agent_control()
            .agent_check_landing_candidate(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_get_provider_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::agent_provider_status::AgentGetProviderStatusRequestV1 = if request
            .params
            .is_null()
        {
            Default::default()
        } else {
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("provider_status_invalid_request".into()))?
        };
        let result = self
            .session_manager
            .agent_control()
            .agent_get_provider_status(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_get_daemon_info(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        if !request.params.is_null() {
            rsi_common::harness_manager::decode_manager_request::<
                rsi_common::agent_daemon_info::AgentGetDaemonInfoRequestV1,
            >(request.params.clone())
            .map_err(|_| DaemonError::InvalidParam("daemon_info_invalid_request".into()))?;
        }
        let result = self
            .session_manager
            .agent_control()
            .agent_get_daemon_info(caller)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_push_satellite_manager_policy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite_manager_policy::*;
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentPushSatelliteManagerPolicyRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam(POLICY_INVALID.into()))?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_push_satellite_manager_policy(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// #2750: the hub manager's reseat of a paired satellite's failed seat.
    pub(super) async fn handle_agent_reseat_satellite_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::manager_reseat::*;
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentReseatSatelliteManagerRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam(SATELLITE_RESEAT_INVALID.into()))?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_reseat_satellite_manager(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    pub(super) async fn handle_agent_request_deploy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::agent_deploy::AgentRequestDeployRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("deploy_invalid_request".into()))?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_request_deploy(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    pub(super) async fn handle_agent_send_satellite_message(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::satellite_dispatch::AgentSendSatelliteMessageRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("satellite_message_invalid".into()))?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_send_satellite_message(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    pub(super) async fn handle_agent_report_to_hub(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: rsi_common::satellite_dispatch::AgentReportToHubRequestV1 =
            rsi_common::harness_manager::decode_manager_request(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam("satellite_report_invalid".into()))?;
        let receipt = self
            .session_manager
            .agent_control()
            .agent_report_to_hub(caller, params)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// Issue #633: the six scoped `AgentTopology*` verbs. The caller is
    /// token-bound; authority, policy and audit live behind
    /// `SessionManager::agent_topology_call`, shared with the native tools.
    pub(super) async fn handle_agent_topology(
        &self,
        request: &RpcRequest,
        verb: AgentControlVerbV1,
    ) -> Result<serde_json::Value> {
        let caller = self
            .resolve_caller_session_id(request)
            .await
            .map_err(|_| crate::session::topology_agent_verbs::unattributed())?;
        self.session_manager
            .agent_topology_call(caller, verb, &request.params)
            .await
    }

    pub(super) async fn handle_agent_get_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        // `request.params` legitimately defaults to `Null` when a caller
        // omits `--params` entirely (this is the documented way to target
        // "myself"), and a bare `Null` fails struct deserialization even
        // though every field here is optional. Only that specific absent-
        // params case should fall back to defaults; anything else that
        // fails to deserialize (wrong type, malformed UUID string, ...) is
        // a genuine caller error and must not be silently reinterpreted as
        // "target self" (see review finding: this previously masked a
        // malformed `session_id` as a self-target no-op).
        let params: AgentGetStatusParams = if request.params.is_null() {
            AgentGetStatusParams::default()
        } else {
            // #2312: `session_id` may be a unique prefix inside the caller's scope.
            let expanded = self
                .session_manager
                .agent_control()
                .expand_id_prefixes(
                    caller_session_id,
                    AgentControlVerbV1::GetStatus,
                    request.params.clone(),
                )
                .await?;
            serde_json::from_value(expanded)
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };
        let target_session_id = params.session_id.unwrap_or(caller_session_id);

        let session = self
            .session_manager
            .agent_get_status(caller_session_id, target_session_id)
            .await?;
        Ok(serde_json::to_value(&session)?)
    }

    /// `AgentReadSessionEvents` (#1041): read-only, scoped like `AgentGetStatus`.
    pub(super) async fn handle_agent_read_session_events(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        // #2312: `session_id` may be a unique prefix inside the caller's scope.
        let expanded = self
            .session_manager
            .agent_control()
            .expand_id_prefixes(
                caller_session_id,
                AgentControlVerbV1::ReadSessionEvents,
                request.params.clone(),
            )
            .await?;
        let params: rsi_common::agent_session_events::AgentReadSessionEventsRequestV1 =
            serde_json::from_value(expanded)
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_read_session_events(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(&result)?)
    }

    pub(super) async fn handle_agent_halt(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        // See the matching comment in `handle_agent_get_status`: only a
        // bare-absent (`Null`) params value defaults to "target self";
        // malformed non-null params must error, not silently self-target.
        let params: AgentHaltParams = if request.params.is_null() {
            AgentHaltParams::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?
        };
        let target_session_id = params.session_id.unwrap_or(caller_session_id);

        self.session_manager
            .agent_halt(caller_session_id, target_session_id)
            .await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// `AgentContinueChild` is a strict-params verb: unlike
    /// `AgentGetStatus`/`AgentHalt` there is no "target self" default to fall
    /// back to, because self-continuation is refused outright. Absent params
    /// are therefore an error, not an implicit self-target.
    pub(super) async fn handle_agent_continue_child(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentContinueChildRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let result = self
            .session_manager
            .agent_continue_child(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    /// The current Epic lead alone may archive a child. Caller identity comes
    /// from the transport token, never from request params.
    pub(super) async fn handle_agent_archive_child(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentArchiveChildRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|_| {
                crate::error::agent_archive_invalid_request("agent_archive_invalid_params")
            })?;
        let result = self
            .session_manager
            .agent_archive_child(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    /// `AgentScheduleWake` — session-attributed wake scheduling. Reuses the
    /// shared `build_scheduled_job` validation/construction helper
    /// (`session/harness/tools/schedule_wake.rs`) so this and the
    /// `schedule_wake` harness tool produce equivalent `ScheduledJob` rows
    /// for equivalent input. `origin_session_id` is always the resolved
    /// caller — never a request field — so `mode:"resume"` can only ever
    /// resume the caller's own session, and an A8 `mode:"on_terminal"` watch
    /// can only ever wake the caller (worst case: self-DoS, bounded by the
    /// per-master cap and natural-key dedup in the shared arm service).
    pub(super) async fn handle_agent_schedule_wake(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use crate::session::agent_verbs::{ArmWatchOutcome, ProgramGuardRegistration};

        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentScheduleWakeParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let caller = self
            .session_manager
            .get_session(caller_session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(caller_session_id))?;

        // #1006: a daemon-evaluated predicate wake. Its own arm service binds
        // the wake to the caller and validates the job ids under one lock.
        if params.mode.as_deref() == Some("when") {
            use rsi_common::wake_predicate as wp;
            if params.in_seconds.is_some() || params.at.is_some() || params.every_seconds.is_some()
            {
                return Err(DaemonError::InvalidParam(
                    wp::WAKE_WHEN_TIMING_UNSUPPORTED.into(),
                ));
            }
            if params.watch_session_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "'watch_session_id' is only valid with mode 'on_terminal'".into(),
                ));
            }
            let predicate = params
                .when
                .ok_or_else(|| DaemonError::InvalidParam(wp::WAKE_WHEN_PREDICATE_INVALID.into()))?;
            let (job, replaced) = self
                .session_manager
                .agent_control()
                .arm_wake_when(
                    caller_session_id,
                    params.message,
                    params.name,
                    predicate.clone(),
                    params.timeout_seconds,
                )
                .await?;
            if let Some(handle) = &self.scheduler_handle {
                // Evaluate now: an already-terminal batch fires without waiting
                // for the next fast-lane pass.
                let _ = handle.check_now().await;
            }
            return Ok(serde_json::json!({
                "job_id": job.id,
                "next_fire_at": job.next_fire_at,
                "wake_mode": job.wake_mode,
                "wake_session_id": job.wake_session_id,
                "when": predicate,
                "replaced_job_ids": replaced,
            }));
        }
        if params.when.is_some() || params.timeout_seconds.is_some() {
            return Err(DaemonError::InvalidParam(
                rsi_common::wake_predicate::WAKE_WHEN_FIELD_MISPLACED.into(),
            ));
        }

        let is_watch = params.mode.as_deref() == Some("on_terminal");
        let is_program_guard = params.mode.as_deref() == Some("program_guard");
        let watched: Option<Uuid> = if is_watch {
            let raw = params.watch_session_id.as_deref().ok_or_else(|| {
                DaemonError::InvalidParam("mode 'on_terminal' requires 'watch_session_id'".into())
            })?;
            let watched = Uuid::parse_str(raw)
                .map_err(|e| DaemonError::InvalidParam(format!("invalid watch_session_id: {e}")))?;
            // #2725: the caller's own queued Issue-worker launch has no session
            // row yet, and the launch arms this very watch when it is
            // established (or ends). Answer covered, not `session_not_found`.
            if self
                .session_manager
                .agent_control()
                .issue_worker_launch_watch_pending(caller_session_id, watched)
                .await
            {
                return Ok(serde_json::json!({
                    "deduplicated": true,
                    "watch": "pending_launch",
                    "wake_mode": { "on_terminal": watched },
                    "wake_session_id": caller_session_id,
                    "note": rsi_common::manager_issue_worker::ISSUE_WORKER_WATCH_PENDING_NOTE,
                }));
            }
            // Watched-subject scope = AgentGetStatus/AgentHalt scope
            // (self / direct child / child-of-led-Epic), self-watch rejected.
            self.session_manager
                .authorize_watch_target(caller_session_id, watched)
                .await?;
            Some(watched)
        } else {
            if params.watch_session_id.is_some() {
                return Err(DaemonError::InvalidParam(
                    "'watch_session_id' is only valid with mode 'on_terminal'".into(),
                ));
            }
            None
        };

        // One enabled job per explicit name per session: an explicit `name`
        // replaces the caller's earlier enabled job of that name. The default
        // (unnamed) wakes keep coexisting.
        let replace_name = params.name.is_some() && !is_program_guard;
        let req = crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
            message: params.message,
            in_seconds: params.in_seconds,
            at: params.at,
            name: params.name,
            every_seconds: params.every_seconds,
            mode: params.mode,
            working_dir: if is_program_guard {
                caller
                    .sandbox_root
                    .clone()
                    .unwrap_or_else(|| caller.working_dir.clone())
            } else {
                caller.working_dir.clone()
            },
            provider: Some(caller.provider),
            model: caller.model.clone(),
            project_id: caller.project_id,
            origin_session_id: Some(caller_session_id),
            watch_session_id: watched,
        };

        let job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(req)
            .map_err(DaemonError::InvalidParam)?;

        if is_program_guard {
            return match self
                .session_manager
                .agent_control()
                .register_program_guard(caller_session_id, job)
                .await?
            {
                ProgramGuardRegistration::Registered(job) => Ok(serde_json::json!({
                    "job_id": job.id,
                    "next_fire_at": job.next_fire_at,
                    "wake_mode": job.wake_mode,
                    "wake_session_id": job.wake_session_id,
                })),
                ProgramGuardRegistration::Deduplicated(job) => Ok(serde_json::json!({
                    "job_id": job.id,
                    "next_fire_at": job.next_fire_at,
                    "wake_mode": job.wake_mode,
                    "wake_session_id": job.wake_session_id,
                    "deduplicated": true,
                })),
            };
        }

        if is_watch {
            // A8.1 F-2: dedup + cap + insert run as ONE store-lock critical
            // section in the shared arm service — the same path the harness
            // tool uses — so concurrent identical arms cannot double-insert.
            let armed = if replace_name {
                self.session_manager
                    .agent_control()
                    .arm_terminal_watch_replacing_name(caller_session_id, job)
                    .await?
            } else {
                self.session_manager
                    .agent_control()
                    .arm_terminal_watch(caller_session_id, job)
                    .await?
            };
            return match armed {
                ArmWatchOutcome::Armed(job) => {
                    self.nudge_armed_watch(&job).await;
                    Ok(serde_json::json!({
                        "job_id": job.id,
                        "next_fire_at": job.next_fire_at,
                        "wake_mode": job.wake_mode,
                        "wake_session_id": job.wake_session_id,
                        "rearmed": false,
                    }))
                }
                // #1391: the existing row delivered an epoch the child has
                // left; it now owes the child's next terminal state.
                ArmWatchOutcome::Rearmed(job) => {
                    self.nudge_armed_watch(&job).await;
                    Ok(serde_json::json!({
                        "job_id": job.id,
                        "next_fire_at": job.next_fire_at,
                        "wake_mode": job.wake_mode,
                        "wake_session_id": job.wake_session_id,
                        "deduplicated": true,
                        "rearmed": true,
                    }))
                }
                // Idempotent re-arm: the EXISTING live row, no insert, no nudge.
                ArmWatchOutcome::Deduplicated(existing) => Ok(serde_json::json!({
                    "job_id": existing.id,
                    "next_fire_at": existing.next_fire_at,
                    "wake_mode": existing.wake_mode,
                    "wake_session_id": existing.wake_session_id,
                    "deduplicated": true,
                    "rearmed": false,
                })),
            };
        }

        let replaced: Vec<Uuid> = {
            let store = self.session_manager.store().lock().await;
            if replace_name {
                store
                    .insert_scheduled_job_replacing_name(caller_session_id, &job)
                    .map_err(|e| DaemonError::Rpc(format!("failed to create scheduled job: {e}")))?
            } else {
                store.insert_scheduled_job(&job).map_err(|e| {
                    DaemonError::Rpc(format!("failed to create scheduled job: {e}"))
                })?;
                Vec::new()
            }
        };

        Ok(serde_json::json!({
            "job_id": job.id,
            "next_fire_at": job.next_fire_at,
            "wake_mode": job.wake_mode,
            "wake_session_id": job.wake_session_id,
            "replaced_job_ids": replaced,
        }))
    }

    /// `AgentListWakes` — scheduled jobs visible to the caller, bounded. Caller
    /// identity comes from the RPC transport; optional `session_id` defaults
    /// to the caller and may target another session within the roster scope.
    pub(super) async fn handle_agent_list_wakes(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentListWakesParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_list_wakes(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(&result)?)
    }

    /// `AgentCancelWake` — disable the caller's own scheduled job(s) by
    /// `job_id` or `name`. The owner is the resolved caller session, never a
    /// request field; another session's job is indistinguishable from a
    /// missing one. Rows are disabled through the store, never deleted.
    pub(super) async fn handle_agent_cancel_wake(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentCancelWakeParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        if !params.selector_is_valid() {
            return Err(DaemonError::InvalidParam(
                "provide exactly one of 'job_id' or a nonblank 'name' (max 256 bytes)".into(),
            ));
        }
        let cancelled = {
            let store = self.session_manager.store().lock().await;
            let _admission =
                crate::session::rpc_admission::admit_current_rpc(caller_session_id).await?;
            store.cancel_owned_scheduled_jobs(
                caller_session_id,
                params.job_id,
                params.name.as_deref(),
            )?
        };
        Ok(serde_json::json!({
            "cancelled_job_ids": cancelled,
            "cancelled_count": cancelled.len(),
        }))
    }

    /// A8: post-insert nudge for a freshly armed watch. Best-effort
    /// `trigger_now` closes the arm-time race (§3.3: an already-terminal
    /// child fires on the first attempt instead of waiting out the reconcile
    /// tick); with the scheduler disabled the arm still stands — warn and
    /// surface, the watch activates when the scheduler is re-enabled.
    pub(super) async fn nudge_armed_watch(&self, job: &rsi_common::types::ScheduledJob) {
        if let Some(handle) = &self.scheduler_handle {
            let _ = handle.trigger_now(job.id).await;
        } else {
            tracing::warn!(
                job_id = %job.id,
                "terminal watch armed while scheduler is disabled; it will not fire until RSI_SCHEDULER_ENABLED=true"
            );
            self.session_manager
                .event_bus()
                .publish(crate::bus::DaemonEvent::SystemMessage {
                    level: "warn".into(),
                    message: format!(
                        "terminal watch '{}' (id={}) armed while the scheduler is disabled; it activates when the scheduler is re-enabled",
                        job.name, job.id
                    ),
                });
        }
    }
}
