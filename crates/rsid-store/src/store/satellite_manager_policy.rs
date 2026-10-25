//! Satellite-side policy CAS and provenance, using the existing policy journal.
use super::Store;
use crate::error::{DaemonError, Result};
use rsi_common::SessionStatus;
use rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2;
use rsi_common::satellite_dispatch::SATELLITE_TARGET_NOT_AUTHORIZED;
use rsi_common::satellite_manager_policy::*;
use rusqlite::{Transaction, TransactionBehavior};

fn denied() -> DaemonError {
    DaemonError::PolicyDenied(SATELLITE_TARGET_NOT_AUTHORIZED.into())
}
impl Store {
    /// Effective launch-choice count per live manager seat (#1941), read from
    /// the saved policy and live ancestors only: no schema or authority change.
    /// A revoked or paused-out policy (its seat rotated away) is skipped.
    pub fn satellite_manager_launches(&self) -> Result<Vec<SatelliteManagerLaunchesV1>> {
        let mut rows = Vec::new();
        let mut after = None;
        'pages: loop {
            let page = self.manager_v2_projects_after(after, 32)?;
            let Some(last) = page.last().copied() else {
                break;
            };
            for project in page {
                if let Some(row) = self.satellite_manager_launch_row(project, false)? {
                    rows.push(row);
                }
                if rows.len() >= SATELLITE_MAX_MANAGER_LAUNCH_ROWS {
                    break 'pages;
                }
            }
            after = Some(last);
        }
        Ok(rows)
    }

    /// Every declared project, including vacant seats and absent policies.
    /// Kept separate from the legacy eight-row summary for old hub validators.
    pub fn satellite_manager_policies(&self) -> Result<Vec<SatelliteManagerLaunchesV1>> {
        let mut rows = Vec::new();
        for project in self.satellite_inbound_policy()?.scope_projects {
            if let Some(row) = self.satellite_manager_launch_row(project.0, true)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    fn satellite_manager_launch_row(
        &self,
        project: uuid::Uuid,
        declared: bool,
    ) -> Result<Option<SatelliteManagerLaunchesV1>> {
        let policy = self.get_harness_manager_policy(project)?;
        let config = self.get_harness_manager_notice_config(project)?;
        if !declared && (policy.as_ref().is_none_or(|p| p.revoked) || config.is_none()) {
            return Ok(None);
        }
        let allowed = policy
            .as_ref()
            .map(|p| p.policy.allowed_launches.as_slice());
        let live_config = config
            .as_ref()
            .filter(|c| !c.is_revoked() && !policy.as_ref().is_some_and(|p| p.revoked));
        let launches = match live_config {
            Some(config) => self
                .manager_effective_launches(config, allowed.unwrap_or_default())?
                .len(),
            None => 0,
        };
        Ok(Some(SatelliteManagerLaunchesV1 {
            project_id: project,
            launches: u32::try_from(launches).unwrap_or(u32::MAX),
            manager_session_id: live_config.and_then(|c| c.current_session_id),
            scope_version: config.as_ref().map(|c| c.row_version),
            policy_version: Some(policy.as_ref().map_or(0, |p| p.row_version)),
            allowed_launches: Some(
                u32::try_from(allowed.unwrap_or_default().len()).unwrap_or(u32::MAX),
            ),
        }))
    }

    /// #2750: the project whose manager seat a hub's reseat names. The same
    /// gate as a policy push: an allowlisted hub, the declared address in the
    /// satellite's own inbound scope resolved to a live seat (a session
    /// root's rotation tip, or a project's current appointed manager), and
    /// that seat the project's appointed manager. A seat that is already a
    /// healthy successor passes here and is refused by the reseat plan.
    pub fn hub_manager_reseat_project(
        &self,
        wire: &rsi_common::manager_reseat::SatelliteManagerReseatRequestV1,
    ) -> Result<uuid::Uuid> {
        wire.validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let scope = self.inbound_scope()?;
        if !scope.allows_hub(wire.hub_installation_id.0) {
            return Err(denied());
        }
        let seat = scope
            .seat_for_address(wire.request.remote_session_id.0)
            .ok_or_else(denied)?;
        let session = self
            .get_session(seat)?
            .filter(|s| !matches!(s.status, SessionStatus::Archived | SessionStatus::Deleted))
            .ok_or_else(denied)?;
        let (config, is_manager) = self
            .manager_config_for_caller(session.id)
            .map_err(|_| denied())?;
        if !is_manager || config.is_revoked() {
            return Err(denied());
        }
        Ok(config.project_id)
    }

    /// #3028: whether a hub has pushed a manager policy for `project`
    /// (`apply_hub_manager_policy` journals a `satellite_policy` event per push).
    pub(crate) fn project_policy_pushed_by_hub(&self, project: uuid::Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_events WHERE project_id=?1 AND kind='satellite_policy')",
            [project.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn apply_hub_manager_policy(
        &self,
        wire: &SatelliteManagerPolicyRequestV1,
    ) -> Result<SatelliteManagerPolicyReceiptV1> {
        wire.validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // #1928: the same resolver as delivery, deploy and reports. The
        // declared address (a root or a project) resolves to its live seat.
        let scope = self.inbound_scope()?;
        let request = &wire.request;
        if !scope.allows_hub(wire.hub_installation_id.0) {
            return Err(denied());
        }
        let seat = scope
            .seat_for_address(request.remote_session_id.0)
            .ok_or_else(denied)?;
        let session = self
            .get_session(seat)?
            .filter(|s| !matches!(s.status, SessionStatus::Archived | SessionStatus::Deleted))
            .ok_or_else(denied)?;
        let (config, is_manager) = self
            .manager_config_for_caller(session.id)
            .map_err(|_| denied())?;
        if !is_manager || config.is_revoked() {
            return Err(denied());
        }
        // Namespace keys by the authenticated hub and sending seat. Hashing
        // keeps them bounded and prevents collisions with operator saves.
        let identity = serde_json::json!([
            wire.hub_installation_id,
            wire.sender_session_id,
            request.idempotency_key
        ]);
        let hash = super::super::harness_manager_v2::fingerprint(&identity)?;
        let key = format!("sat-policy:{hash}");
        let payload = serde_json::to_value(wire)?;
        // Receipts belong to the scope admitted by the original request. An
        // exact retry after a scope edit returns that outcome without writing
        // policy again; current inbound and manager authority still gate it.
        let mut replay_config = config.clone();
        replay_config.row_version = request.expected_scope_version;
        if let Some(replay) = self
            .manager_v2_replay(&replay_config, &key, &payload)
            .map_err(policy_error)?
        {
            return Ok(serde_json::from_value(replay)?);
        }
        let previous = self.get_harness_manager_policy(config.project_id)?;
        let revoked = previous.as_ref().is_some_and(|p| p.revoked);
        let version = previous.as_ref().map_or(0, |p| p.row_version);
        // Keep the original refusal precedence: a scope edit is stale even
        // when it revoked the policy; with a matching scope, revocation wins.
        if config.row_version != request.expected_scope_version
            || (!revoked && version != request.expected_policy_version)
        {
            let error = SatelliteManagerPolicyStaleV1 {
                actual_scope_version: Some(config.row_version),
                actual_policy_version: Some(version),
                allowed_launches: Some(
                    u32::try_from(
                        previous
                            .as_ref()
                            .map_or(0, |p| p.policy.allowed_launches.len()),
                    )
                    .unwrap_or(u32::MAX),
                ),
                manager_session_id: Some(session.id),
            }
            .rpc_error();
            return Err(DaemonError::StructuredRpc {
                rpc_code: error.code,
                message: error.message,
                data: error.data.unwrap_or_default(),
            });
        }
        if revoked {
            return Err(denied());
        }
        let mut policy = previous.map(|p| p.policy).unwrap_or_default();
        request.policy.apply(&mut policy);
        let result = self.configure_harness_manager_policy_in_tx(
            &tx,
            &ConfigureHarnessManagerPolicyRequestV2 {
                project_id: config.project_id,
                expected_scope_version: config.row_version,
                expected_policy_version: version,
                idempotency_key: format!("sat-save:{hash}"),
                policy,
            },
        )?;
        let receipt = SatelliteManagerPolicyReceiptV1 {
            hub_installation_id: wire.hub_installation_id,
            sender_session_id: wire.sender_session_id,
            config: result,
            deduplicated: false,
        };
        let value = serde_json::to_value(&receipt)?;
        self.manager_v2_event(
            &config,
            None,
            "satellite_policy",
            &key,
            receipt.config.row_version,
            &value,
        )?;
        self.manager_v2_save_receipt(
            &config,
            None,
            receipt.config.row_version,
            "satellite_policy",
            &key,
            &payload,
            &value,
        )?;
        tx.commit()?;
        Ok(receipt)
    }
}
fn policy_error(error: DaemonError) -> DaemonError {
    match error {
        DaemonError::InvalidParam(code) if code == "manager_v2_idempotency_conflict" => {
            DaemonError::PolicyDenied(POLICY_KEY_CONFLICT.into())
        }
        other => other,
    }
}
