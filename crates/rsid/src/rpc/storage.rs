use super::*;

/// Operator-only bounded target-cache maintenance request (Issue #69).
#[derive(Debug, Deserialize)]
pub struct RunSandboxBuildCacheReclaimParams {
    pub dry_run: bool,
}

/// Operator-only bounded absent-root worktree adoption request.
#[derive(Debug, Deserialize)]
pub struct RunSandboxWorktreeReclaimParams {
    pub dry_run: bool,
    pub max_count: u32,
}

/// Operator-only bounded archived sandbox purge (Issue #955).
///
/// `after` resumes from a previous report's `next_cursor`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunArchivedSandboxPurgeParams {
    pub dry_run: bool,
    pub max_count: u32,
    #[serde(default)]
    pub after: Option<crate::session::sandbox_purge::ArchivedSandboxPurgeCursor>,
}

impl RpcServer {
    pub(super) async fn handle_list_source_worktree_cohorts(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _: ListSourceWorktreeCohortsParams = serde_json::from_value(request.params.clone())
            .map_err(|error| {
                DaemonError::InvalidParam(format!(
                    "invalid ListSourceWorktreeCohorts params: {error}"
                ))
            })?;
        serde_json::to_value(self.session_manager.list_source_worktree_cohorts().await?)
            .map_err(Into::into)
    }

    pub(super) async fn handle_audit_source_worktree_cohort(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: AuditSourceWorktreeCohortParams =
            serde_json::from_value(request.params.clone()).map_err(|error| {
                DaemonError::InvalidParam(format!(
                    "invalid AuditSourceWorktreeCohort params: {error}"
                ))
            })?;
        params.validate().map_err(DaemonError::InvalidParam)?;
        serde_json::to_value(
            self.session_manager
                .audit_source_worktree_cohort(params.repository_identity)
                .await?,
        )
        .map_err(Into::into)
    }

    pub(super) async fn handle_apply_source_worktree_cohort(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ApplySourceWorktreeCohortParams =
            serde_json::from_value(request.params.clone()).map_err(|error| {
                DaemonError::InvalidParam(format!(
                    "invalid ApplySourceWorktreeCohort params: {error}"
                ))
            })?;
        params.validate().map_err(DaemonError::InvalidParam)?;
        serde_json::to_value(
            self.session_manager
                .apply_source_worktree_cohort(params)
                .await?,
        )
        .map_err(Into::into)
    }

    pub(super) async fn handle_get_source_worktree_settlement_run(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetSourceWorktreeSettlementRunParams =
            serde_json::from_value(request.params.clone()).map_err(|error| {
                DaemonError::InvalidParam(format!(
                    "invalid GetSourceWorktreeSettlementRun params: {error}"
                ))
            })?;
        params.validate().map_err(DaemonError::InvalidParam)?;
        serde_json::to_value(
            self.session_manager
                .get_source_worktree_settlement_run(params.run_id)
                .await?,
        )
        .map_err(Into::into)
    }

    pub(super) async fn handle_get_archive_cleanup_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetArchiveCleanupStatusParamsV1 =
            serde_json::from_value(request.params.clone()).map_err(|_| {
                DaemonError::InvalidParam("invalid GetArchiveCleanupStatus request".into())
            })?;
        let status = self
            .session_manager
            .get_archive_cleanup_status(params.session_id)
            .await?;
        status.validate_wire().map_err(DaemonError::Store)?;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_get_sandbox_storage_status(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let report = self
            .session_manager
            .run_sandbox_build_cache_reclaim_wire_for_trigger(true, "preview")
            .await?;
        Ok(serde_json::to_value(report)?)
    }

    pub(super) async fn handle_run_sandbox_build_cache_reclaim(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RunSandboxBuildCacheReclaimParams =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::InvalidParam(format!("Invalid params: {error}")))?;
        let trigger = if params.dry_run {
            "operator_dry_run"
        } else {
            "operator_actual"
        };
        let report = self
            .session_manager
            .run_sandbox_build_cache_reclaim_wire_for_trigger(params.dry_run, trigger)
            .await?;
        Ok(serde_json::to_value(report)?)
    }

    pub(super) async fn handle_run_sandbox_worktree_reclaim(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RunSandboxWorktreeReclaimParams =
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::InvalidParam(format!("Invalid params: {error}")))?;
        if !(1..=1024).contains(&params.max_count) {
            return Err(DaemonError::InvalidParam(
                "max_count must be in 1..=1024".into(),
            ));
        }
        self.session_manager
            .run_sandbox_worktree_reclaim(params.dry_run, params.max_count as usize)
            .await
    }

    pub(super) async fn handle_run_archived_sandbox_purge(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RunArchivedSandboxPurgeParams = serde_json::from_value(request.params.clone())
            .map_err(|error| DaemonError::InvalidParam(format!("Invalid params: {error}")))?;
        if !(1..=1024).contains(&params.max_count) {
            return Err(DaemonError::InvalidParam(
                "max_count must be in 1..=1024".into(),
            ));
        }
        let report = self
            .session_manager
            .run_archived_sandbox_purge(params.dry_run, params.max_count as usize, params.after)
            .await?;
        Ok(serde_json::to_value(report)?)
    }
}
