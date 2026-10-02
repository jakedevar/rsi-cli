//! K14 (#672): executor for the delegated `operator_call` manager action.
//!
//! Only the closed `DelegatedOperatorCallV1` allowlist is executable, and it
//! never reaches the RPC dispatcher. Reads call the same `SessionManager`
//! entry the operator handler calls; the logical `ArchiveSession` uses the
//! manager-specific guarded path and never runs archive cleanup.
use super::SessionManager;
use crate::error::{DaemonError, Result};
use crate::store::harness_manager_v2::refused;
use crate::store::manager_actions::ManagerActionClaimV2;
use rsi_common::harness_manager_v2::ManagerActionV2;
use rsi_common::manager_operator_delegation::{DelegatedOperatorCallV1, OperatorCallResultV1};

/// Heap-allocated delegated executor future.
pub(super) type DelegatedOperatorFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

impl SessionManager {
    /// Construct the delegated executor future on the heap, outside the
    /// caller's async state machine and poll frame. It applies the same
    /// runtime gate every lifecycle action passes before its effect.
    #[inline(never)]
    pub(super) fn boxed_delegated_operator_call<'a>(
        &'a self,
        claim: &'a ManagerActionClaimV2,
    ) -> DelegatedOperatorFuture<'a> {
        Box::pin(async move {
            self.check_manager_action_runtime(claim, false).await?;
            self.execute_delegated_operator_call(claim).await
        })
    }

    async fn finish_delegated_storage_result(
        &self,
        claim: &ManagerActionClaimV2,
        method: &str,
        report: rsi_common::sandbox_storage::SandboxBuildCacheReclaimReportWire,
    ) -> Result<()> {
        let result = OperatorCallResultV1::Scalar {
            method: method.into(),
            result: serde_json::to_value(report)?,
        };
        // A report that cannot fit the receipt bound is refused, not truncated.
        result.validate().map_err(refused)?;
        self.store
            .lock()
            .await
            .finish_delegated_read(claim, result)?;
        Ok(())
    }

    /// #1046: apply one allowlisted setting inside the operator's bounds. The
    /// receipt carries the key, previous and new value and the reason, so the
    /// manager action journal (and the TUI Actions view) is the audit trail.
    async fn execute_delegated_setting_proposal(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let params = self.store.lock().await.authorize_delegated_setting(claim)?;
        let previous = {
            let _guard = self.daemon_config_update.lock().await;
            self.apply_manager_daemon_setting(&params.key, params.value)
                .await?
        };
        tracing::info!(
            key = %params.key,
            previous = previous,
            value = params.value,
            reason = %params.reason,
            "manager changed a daemon setting within operator bounds"
        );
        let result = OperatorCallResultV1::Scalar {
            method: "ProposeDaemonSetting".into(),
            result: serde_json::json!({
                "key": params.key,
                "previous": previous,
                "value": params.value,
                "reason": params.reason,
            }),
        };
        result.validate().map_err(refused)?;
        self.store
            .lock()
            .await
            .finish_delegated_read(claim, result)?;
        Ok(())
    }

    /// Write one curated setting through the same validate, persist, publish
    /// steps as the operator `UpdateDaemonConfig` handler. The caller holds
    /// `daemon_config_update`. Returns the previous value.
    async fn apply_manager_daemon_setting(&self, key: &str, value: u64) -> Result<u64> {
        let json_value = serde_json::json!(value);
        let reject = |error: String| {
            tracing::warn!(key, value, %error, "daemon rejected a manager setting proposal");
            refused("manager_v2_daemon_setting_rejected")
        };
        let cfg = &self.runtime_config;
        let watermark = matches!(
            key,
            "sandbox_build_cache_reclaim_high_watermark_pct"
                | "sandbox_build_cache_reclaim_low_watermark_pct"
        );
        if watermark {
            let snapshot = cfg.sandbox_build_cache_reclaim_snapshot();
            let previous = if key.contains("high") {
                snapshot.high_watermark_pct
            } else {
                snapshot.low_watermark_pct
            };
            let prepared = cfg
                .prepare_sandbox_build_cache_update(key, &json_value)
                .map_err(reject)?
                .ok_or_else(|| refused("manager_v2_daemon_setting_not_allowlisted"))?;
            {
                let store = self.store.lock().await;
                crate::store::daemon_settings::persist_sandbox_build_cache_config_update(
                    &store, key, prepared,
                )?;
            }
            cfg.publish_sandbox_build_cache_config(prepared);
            return Ok(u64::from(previous));
        }
        let previous_value = cfg.to_json()[key].clone();
        // Every allowlisted key is an unsigned integer. Refuse rather than
        // journal a made-up previous value if that ever stops being true.
        let previous = previous_value
            .as_u64()
            .ok_or_else(|| refused("manager_v2_daemon_setting_not_allowlisted"))?;
        if !cfg.update_field(key, &json_value).map_err(reject)? {
            return Err(refused("manager_v2_daemon_setting_not_allowlisted"));
        }
        let persisted = {
            let store = self.store.lock().await;
            crate::store::daemon_settings::persist_runtime_config_field(&store, cfg, key)
        };
        if let Err(error) = persisted {
            // Roll the live value back so memory and the database agree.
            if let Err(rollback) = cfg.update_field(key, &previous_value) {
                tracing::warn!(
                    key,
                    previous,
                    error = %rollback,
                    "manager daemon setting rollback failed after a persist error; \
                     the live value and the database may disagree"
                );
            }
            return Err(error);
        }
        Ok(previous)
    }

    async fn execute_delegated_operator_call(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let ManagerActionV2::OperatorCall { call, .. } = claim.action() else {
            return Err(refused("manager_v2_not_operator_call"));
        };
        match call.typed().map_err(refused)? {
            DelegatedOperatorCallV1::ArchiveSession(params) => {
                let id = params.session_id;
                // A pending retry is cancelled by the archive transaction.
                // A successor already established by retry retains its owner.
                let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
                if self.active.read().await.contains_key(&id) {
                    return Err(refused("manager_v2_session_active"));
                }
                if self
                    .completed
                    .read()
                    .await
                    .get(&id)
                    .is_some_and(|cs| cs.superseded_by_retry.is_some())
                {
                    return Err(refused("manager_v2_human_or_recovery_owner"));
                }
                self.store.lock().await.apply_delegated_archive(claim)?;
                self.project_manager_cascade(vec![id], false).await?;
            }
            DelegatedOperatorCallV1::UnarchiveSession(params) => {
                let id = params.session_id;
                let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
                if self.active.read().await.contains_key(&id) {
                    return Err(refused("manager_v2_session_active"));
                }
                self.store.lock().await.apply_delegated_unarchive(claim)?;
                // Same in-memory projection as the housekeeping restore.
                self.project_manager_cascade(vec![id], true).await?;
            }
            DelegatedOperatorCallV1::GetArchiveCleanupStatus(params) => {
                let status = self.get_archive_cleanup_status(params.session_id).await?;
                status.validate_wire().map_err(DaemonError::Store)?;
                let result = OperatorCallResultV1::Scalar {
                    method: "GetArchiveCleanupStatus".into(),
                    result: serde_json::to_value(status)?,
                };
                self.store
                    .lock()
                    .await
                    .finish_delegated_read(claim, result)?;
            }
            DelegatedOperatorCallV1::GetSandboxStorageStatus(_) => {
                // Same bounded preview the operator status RPC runs.
                let report = self
                    .run_sandbox_build_cache_reclaim_wire_for_trigger(true, "preview")
                    .await?;
                self.finish_delegated_storage_result(claim, "GetSandboxStorageStatus", report)
                    .await?;
            }
            DelegatedOperatorCallV1::RunSandboxBuildCacheReclaim(params) => {
                // The configured watermarks and limits apply; the caller only
                // chooses preview or a real pass.
                let trigger = if params.dry_run {
                    "manager_dry_run"
                } else {
                    "manager_actual"
                };
                let report = self
                    .run_sandbox_build_cache_reclaim_wire_for_trigger(params.dry_run, trigger)
                    .await?;
                self.finish_delegated_storage_result(claim, "RunSandboxBuildCacheReclaim", report)
                    .await?;
            }
            DelegatedOperatorCallV1::ProposeDaemonSetting(_) => {
                self.execute_delegated_setting_proposal(claim).await?;
            }
            DelegatedOperatorCallV1::ListSessions(_) => {
                self.store
                    .lock()
                    .await
                    .execute_delegated_list_sessions(claim)?;
            }
        }
        Ok(())
    }
}
