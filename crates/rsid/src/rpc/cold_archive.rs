//! Operator-only RPCs of the cold-storage archive (#2384): a status read, a
//! run-now action, a per-session storage read and a restore. All stay out of
//! `AGENT_VERBS`, `READ_VERBS`, the native tools and the agent CLI catalog: they
//! are reachable only on the operator connection (a session-attributed caller is
//! default-denied). Agents read cold transcripts transparently through
//! `AgentReadSessionEvents` and get no archive controls.

use super::*;
use crate::store::cold_archive::ColdMode;

/// Params of `RunColdArchiveNow`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunColdArchiveNowParams {
    /// `stage` (default) writes and verifies archive chunks; `verify` re-reads
    /// every staged chunk; `move` also deletes the verified rows; `compact`
    /// schedules the compaction for the next supervised restart.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Params of `GetColdSessionStorage` and `RestoreColdSession`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColdSessionParams {
    pub session_id: uuid::Uuid,
}

fn cold_session_params(request: &RpcRequest) -> Result<uuid::Uuid> {
    let params: ColdSessionParams = serde_json::from_value(request.params.clone())
        .map_err(|error| DaemonError::InvalidParam(format!("Invalid params: {error}")))?;
    Ok(params.session_id)
}

impl RpcServer {
    /// `GetColdSessionStorage {session_id}`: `{tier, generation, archived_at,
    /// restored_at, file, archived_events, raw_bytes, compressed_bytes}`; `tier` is
    /// `live`, `cold` or `restored`.
    pub(super) async fn handle_get_cold_session_storage(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let session_id = cold_session_params(request)?;
        let storage = self
            .session_manager
            .cold_session_storage(session_id)
            .await?;
        Ok(serde_json::to_value(storage)?)
    }

    /// `RestoreColdSession {session_id}`: puts a cold session's rows back into the
    /// live tables and returns `{outcome: restored|already_hot, generation, events,
    /// provenance}`; both outcomes are success. Works whether or not the archive
    /// is enabled. A missing or corrupt archive is `cold_archive_unavailable`.
    pub(super) async fn handle_restore_cold_session(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let session_id = cold_session_params(request)?;
        let receipt = self
            .session_manager
            .restore_cold_session(session_id)
            .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    /// `GetColdArchiveStatus {}`: settings, live database size, archive files,
    /// totals, the last run and open problems.
    pub(super) async fn handle_get_cold_archive_status(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let (settings, _database_path) = self.session_manager.cold_archive_settings().await;
        let running = self.session_manager.cold_archive_is_running();
        let store = self.session_manager.store().lock().await;
        let status = store.cold_archive_status(&settings, running)?;
        Ok(serde_json::to_value(status)?)
    }

    /// `RunColdArchiveNow {mode?}`: starts a pass in the background and returns
    /// `{run_id, mode}`. `stage` (default) writes and verifies chunks, `verify`
    /// re-reads them, `move` also deletes the verified rows (only while a fresh
    /// backup exists, checked again before every delete transaction) and reclaims
    /// their pages. `compact` returns `{run_id,
    /// mode, scheduled: true}`: it runs in an isolated helper at the next
    /// supervised restart, never in the live daemon. Refusals:
    /// `cold_archive_disabled`, `cold_archive_running`, and
    /// `cold_archive_shutting_down` while a supervised restart is draining.
    pub(super) async fn handle_run_cold_archive_now(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RunColdArchiveNowParams = if request.params.is_null() {
            RunColdArchiveNowParams { mode: None }
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|error| DaemonError::InvalidParam(format!("Invalid params: {error}")))?
        };
        let mode = match params.mode.as_deref() {
            None => ColdMode::Stage,
            Some(name) => ColdMode::parse(name).ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "Invalid params: unknown mode '{name}' (expected stage, verify, move or compact)"
                ))
            })?,
        };
        if mode == ColdMode::Compact {
            let run_id = self.session_manager.request_cold_compaction().await?;
            return Ok(serde_json::json!({
                "run_id": run_id,
                "mode": mode.as_str(),
                "scheduled": true,
            }));
        }
        let run_id = self
            .session_manager
            .start_cold_archive_run("run_now", mode)
            .await?;
        Ok(serde_json::json!({ "run_id": run_id, "mode": mode.as_str() }))
    }
}
