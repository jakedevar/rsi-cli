//! Operator-only RPCs of the external harness-report intake (#3789).
//!
//! `GetExternalReportSyncStatus {}` returns the last run (repo, time, counts,
//! errors); `SyncExternalReportsNow {}` runs one pass with the hub's own `gh`
//! authentication and returns the new status. Neither is in the attributed verb
//! registry, so a tokened caller is default-denied before dispatch; the
//! handlers refuse one again (AGENTS.md rule 10).

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use rsi_common::external_report::SyncStatusV1;

fn refuse_attributed(method: &str, request: &RpcRequest) -> Result<()> {
    if request.session_token.is_some() {
        return Err(DaemonError::PolicyDenied(format!(
            "operator_only: `{method}` is operator-only and is not available to session-attributed callers"
        )));
    }
    Ok(())
}

impl RpcServer {
    pub(super) async fn handle_get_external_report_sync_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        refuse_attributed("GetExternalReportSyncStatus", request)?;
        let recorded = {
            let store = self.session_manager.store().lock().await;
            store.external_report_sync_status()?
        };
        let status = recorded.unwrap_or_else(|| SyncStatusV1 {
            repo: crate::external_report_sync::configured_repo(),
            ..SyncStatusV1::default()
        });
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_sync_external_reports_now(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        refuse_attributed("SyncExternalReportsNow", request)?;
        let status = crate::external_report_sync::sync_now(&self.session_manager)
            .await
            .map_err(DaemonError::InvalidParam)?;
        Ok(serde_json::to_value(status)?)
    }
}
