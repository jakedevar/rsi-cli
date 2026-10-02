use super::*;

#[allow(clippy::too_many_lines)]
pub(crate) fn program_run_rpc_error(error: &ProgramRunControlError) -> DaemonError {
    let (rpc_code, code, message, retryable) = match error {
        ProgramRunControlError::InvalidRequest => (
            INVALID_PARAMS,
            "PROGRAM_RUN_INVALID_REQUEST",
            "invalid ProgramRun request",
            false,
        ),
        ProgramRunControlError::Forbidden => (
            INVALID_PARAMS,
            "PROGRAM_RUN_FORBIDDEN",
            "ProgramRun operation is forbidden",
            false,
        ),
        ProgramRunControlError::NotFound => (
            INVALID_PARAMS,
            "PROGRAM_RUN_NOT_FOUND",
            "ProgramRun not found",
            false,
        ),
        ProgramRunControlError::ActiveRunExists => (
            INVALID_PARAMS,
            "PROGRAM_RUN_ACTIVE_EXISTS",
            "an active ProgramRun already exists",
            false,
        ),
        ProgramRunControlError::ReplayConflict => (
            INVALID_PARAMS,
            "PROGRAM_RUN_REPLAY_CONFLICT",
            "ProgramRun idempotency replay conflict",
            false,
        ),
        ProgramRunControlError::StaleRunVersion => (
            INVALID_PARAMS,
            "PROGRAM_RUN_STALE_VERSION",
            "stale ProgramRun version",
            true,
        ),
        ProgramRunControlError::StaleIdeaVersion => (
            INVALID_PARAMS,
            "PROGRAM_RUN_STALE_IDEA_VERSION",
            "stale Idea version",
            true,
        ),
        ProgramRunControlError::StaleControllerEpoch
        | ProgramRunControlError::ControllerMismatch => (
            INVALID_PARAMS,
            "PROGRAM_RUN_STALE_AUTHORITY",
            "ProgramRun authority is stale",
            false,
        ),
        ProgramRunControlError::StaleLeaseGeneration
        | ProgramRunControlError::StaleClaimGeneration => (
            INVALID_PARAMS,
            "PROGRAM_RUN_STALE_GENERATION",
            "ProgramRun generation is stale",
            true,
        ),
        ProgramRunControlError::InvalidTransition
        | ProgramRunControlError::CursorInvariant
        | ProgramRunControlError::GateIncomplete
        | ProgramRunControlError::TerminalRun => (
            INVALID_PARAMS,
            "PROGRAM_RUN_INVALID_TRANSITION",
            "ProgramRun transition is not permitted",
            false,
        ),
        ProgramRunControlError::BudgetExhausted => (
            INVALID_PARAMS,
            "PROGRAM_RUN_BUDGET_EXHAUSTED",
            "ProgramRun budget is exhausted",
            false,
        ),
        ProgramRunControlError::QueueBackpressure => (
            INVALID_PARAMS,
            "PROGRAM_RUN_QUEUE_BACKPRESSURE",
            "ProgramRun queue is full",
            true,
        ),
        ProgramRunControlError::LockUnavailable => (
            INVALID_PARAMS,
            "PROGRAM_RUN_LOCK_UNAVAILABLE",
            "ProgramRun lock is unavailable",
            true,
        ),
        ProgramRunControlError::Quarantined | ProgramRunControlError::DownstreamReplayConflict => (
            INVALID_PARAMS,
            "PROGRAM_RUN_QUARANTINED",
            "ProgramRun requires operator reconciliation",
            false,
        ),
        ProgramRunControlError::Contention => (
            INTERNAL_ERROR,
            "PROGRAM_RUN_CONTENTION",
            "ProgramRun storage contention",
            true,
        ),
        ProgramRunControlError::ConstraintViolation
        | ProgramRunControlError::CorruptStoredState
        | ProgramRunControlError::StorageFailure => (
            INTERNAL_ERROR,
            "PROGRAM_RUN_STORAGE_FAILURE",
            "ProgramRun storage failure",
            false,
        ),
    };
    DaemonError::StructuredRpc {
        rpc_code,
        message: message.into(),
        data: serde_json::json!({"code": code, "details": {"retryable": retryable}}),
    }
}

impl RpcServer {
    pub(super) fn program_run_operator(
        &self,
        project_id: Option<Uuid>,
    ) -> Result<BoundProgramRunOperatorAuthority> {
        let handle = self.session_manager.program_run_control_handle();
        project_id.map_or_else(
            || Ok(handle.bind_operator_all()),
            |project_id| {
                handle
                    .bind_operator(project_id)
                    .map_err(|error| program_run_rpc_error(&error))
            },
        )
    }

    pub(super) async fn handle_create_program_run(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateProgramRunParams = serde_json::from_value(request.params.clone())
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let project_id = self
            .session_manager
            .store()
            .lock()
            .await
            .get_idea_with_genesis(params.idea_id)?
            .map(|value| value.idea.project_id)
            .ok_or_else(|| program_run_rpc_error(&ProgramRunControlError::NotFound))?;
        let mutation = self
            .program_run_operator(Some(project_id))?
            .create(&params)
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(CreateProgramRunResult { mutation })?)
    }

    pub(super) async fn handle_get_program_run(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetProgramRunParams = serde_json::from_value(request.params.clone())
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let run = self
            .program_run_operator(None)?
            .get(params.program_run_id)
            .await
            .map_err(|error| program_run_rpc_error(&error))?
            .ok_or_else(|| program_run_rpc_error(&ProgramRunControlError::NotFound))?;
        Ok(serde_json::to_value(run)?)
    }

    pub(super) async fn handle_list_program_runs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListProgramRunsParams = serde_json::from_value(request.params.clone())
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let limit = params
            .validated_limit()
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let page = self
            .program_run_operator(params.project_id)?
            .list(params.idea_id, params.status, params.cursor.as_ref(), limit)
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(ListProgramRunsResult {
            items: page.items,
            next_cursor: page.next_cursor,
        })?)
    }

    pub(super) async fn handle_list_program_run_transitions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListProgramRunTransitionsParams =
            serde_json::from_value(request.params.clone())
                .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let limit = params
            .validated_limit()
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let page = self
            .program_run_operator(None)?
            .transitions(params.program_run_id, params.after_sequence, limit)
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(ListProgramRunTransitionsResult {
            page,
        })?)
    }

    pub(super) async fn handle_create_closure_program(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::closure_kernel::CreateClosureProgramRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::Rpc(format!("Invalid Closure params: {error}")))?;
        Ok(serde_json::to_value(
            crate::closure_kernel::operator::create_program(&self.session_manager, params).await?,
        )?)
    }

    pub(super) async fn handle_update_closure_program(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::closure_kernel::UpdateClosureProgramRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::Rpc(format!("Invalid Closure params: {error}")))?;
        Ok(serde_json::to_value(
            crate::closure_kernel::operator::update_program(&self.session_manager, params).await?,
        )?)
    }

    pub(super) async fn handle_launch_closure_source(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::closure_kernel::LaunchClosureSourceRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::Rpc(format!("Invalid Closure params: {error}")))?;
        Ok(serde_json::to_value(
            crate::closure_kernel::operator::launch_source(&self.session_manager, params).await?,
        )?)
    }

    pub(super) async fn handle_list_closure_programs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::closure_kernel::ListClosureProgramsRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::Rpc(format!("Invalid Closure params: {error}")))?;
        Ok(serde_json::to_value(
            crate::closure_kernel::operator::list_programs(&self.session_manager, params).await?,
        )?)
    }

    pub(super) async fn handle_get_closure_program(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::closure_kernel::GetClosureProgramRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::Rpc(format!("Invalid Closure params: {error}")))?;
        Ok(serde_json::to_value(
            crate::closure_kernel::operator::get_program(&self.session_manager, params).await?,
        )?)
    }

    pub(super) async fn handle_record_closure_evidence(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::closure_kernel::RecordClosureEvidenceRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::Rpc(format!("Invalid Closure params: {error}")))?;
        Ok(serde_json::to_value(
            crate::closure_kernel::operator::record_evidence(&self.session_manager, params).await?,
        )?)
    }

    pub(super) async fn handle_get_program_run_operational_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetProgramRunOperationalStatusParams =
            serde_json::from_value(request.params.clone())
                .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let operator = self.program_run_operator(None)?;
        let run = operator
            .get(params.program_run_id)
            .await
            .map_err(|error| program_run_rpc_error(&error))?
            .ok_or_else(|| program_run_rpc_error(&ProgramRunControlError::NotFound))?;
        let live = self
            .session_manager
            .is_program_run_controller_live(run.controller_session_id)
            .await;
        let status = operator
            .status(params.program_run_id, live)
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(
            GetProgramRunOperationalStatusResult { status },
        )?)
    }

    pub(super) async fn handle_cancel_program_run(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CancelProgramRunParams = serde_json::from_value(request.params.clone())
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let mutation = self
            .program_run_operator(None)?
            .cancel(&params)
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(mutation)?)
    }

    pub(super) async fn handle_resume_blocked_program_run(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ResumeBlockedProgramRunParams = serde_json::from_value(request.params.clone())
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let mutation = self
            .program_run_operator(None)?
            .resume(&params)
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(mutation)?)
    }

    pub(super) async fn handle_reconcile_program_runs(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ReconcileProgramRunsParams = serde_json::from_value(request.params.clone())
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let (limit, time_budget_ms) = params
            .validated_bounds()
            .map_err(|_| program_run_rpc_error(&ProgramRunControlError::InvalidRequest))?;
        let page = self
            .program_run_operator(params.project_id)?
            .reconcile(
                params.cursor.as_ref(),
                limit,
                time_budget_ms,
                params.dry_run,
            )
            .await
            .map_err(|error| program_run_rpc_error(&error))?;
        Ok(serde_json::to_value(ReconcileProgramRunsResult { page })?)
    }
}
