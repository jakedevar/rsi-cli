//! `AgentWorkerRoster` (#2311): a manager reads each worker's real state in
//! one call. The derivation and the scope check live in the store
//! (`Store::agent_worker_roster`); this is the daemon-side entry that the RPC
//! handler and the native tools share.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_worker_roster::{AgentWorkerRosterRequestV1, AgentWorkerRosterResultV1};
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// `worker_roster_invalid_request`, `worker_roster_caller_unavailable`,
    /// `worker_roster_not_authorized`, `manager_project_not_in_scope`, or a
    /// persistence error.
    pub async fn agent_worker_roster(
        &self,
        caller: Uuid,
        request: AgentWorkerRosterRequestV1,
    ) -> Result<AgentWorkerRosterResultV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let mut roster = self
            .store
            .lock()
            .await
            .agent_worker_roster(caller, &request)?;
        if let Some(gate) = &self.host_load {
            for launch in &mut roster.queued_launches {
                launch.held_reason = gate
                    .hold_for_create(launch.operation_id)
                    .map(|hold| hold.reason().to_owned());
            }
        }
        let store = self.store.lock().await;
        if let Some(config) = &self.runtime_config {
            crate::agent_jobs::project_roster_disk_holds(
                &store,
                &config.governor_policy(),
                &mut roster,
            );
        }
        for launch in &mut roster.queued_launches {
            if store
                .sandbox_git_timeout_retry_at(launch.operation_id)?
                .is_some()
            {
                launch.held_reason = Some("sandbox_git_timeout".into());
            }
        }
        Ok(roster)
    }
}
