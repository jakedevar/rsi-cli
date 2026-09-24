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

    async fn execute_delegated_operator_call(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let ManagerActionV2::OperatorCall { call, .. } = claim.action() else {
            return Err(refused("manager_v2_not_operator_call"));
        };
        match call.typed().map_err(refused)? {
            DelegatedOperatorCallV1::ArchiveSession(params) => {
                let id = params.session_id;
                // Same quiescence rules as scoped housekeeping: no concurrent
                // continuation, no live process, no retry/recovery owner.
                let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
                if self.active.read().await.contains_key(&id) {
                    return Err(refused("manager_v2_session_active"));
                }
                if self.completed.read().await.get(&id).is_some_and(|cs| {
                    cs.retry_cancel.is_some()
                        || cs.retry_fired_at.is_some()
                        || cs.superseded_by_retry.is_some()
                }) {
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
