//! `AgentRequestDeploy` (#1045 slice 2): accept one deploy request from the
//! current appointed manager holding the operator-granted `Deploy` capability
//! (Execute mode, not paused), stage and verify the binaries, and record a
//! `staged` row the daemon's deploy runner drives to a quiet-point restart.

use super::agent_verbs::AgentControlHandle;
use crate::deploy::{DeployService, remove_staged};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::agent_deploys::{DeployRow, NewDeploy};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::agent_deploy::{
    AgentRequestDeployReceiptV1, AgentRequestDeployRequestV1, DEPLOY_CAPABILITY_REQUIRED,
    DEPLOY_EXECUTE_REQUIRED, DEPLOY_NEEDS_SUPERVISOR, DEPLOY_NOT_AUTHORIZED, skipped_binaries,
};
use rsi_common::harness_manager_v2::ManagerCapabilityV2;
use rsi_common::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteUuidV1};
use rsi_common::satellite_dispatch::{SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeployRequestV1};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

fn receipt(row: &DeployRow, replayed: bool) -> AgentRequestDeployReceiptV1 {
    AgentRequestDeployReceiptV1 {
        deploy_id: row.id,
        state: row.state,
        sha: row.sha.clone(),
        deadline_at: row.deadline_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        binaries: row.manifest.clone(),
        skipped: skipped_binaries(&row.manifest),
        replayed,
    }
}

impl AgentControlHandle {
    /// # Errors
    /// A stable `deploy_*` refusal, or a persistence error.
    pub async fn agent_request_deploy(
        &self,
        caller: Uuid,
        request: AgentRequestDeployRequestV1,
    ) -> Result<AgentRequestDeployReceiptV1> {
        self.agent_request_deploy_with(caller, request, DeployService::global(), Utc::now())
            .await
    }

    pub(crate) async fn agent_request_deploy_with(
        &self,
        caller: Uuid,
        request: AgentRequestDeployRequestV1,
        service: &DeployService,
        now: DateTime<Utc>,
    ) -> Result<AgentRequestDeployReceiptV1> {
        let root = rsi_common::identity::data_path("satellites", "satellites");
        self.agent_request_deploy_via(caller, request, service, now, &root)
            .await
    }

    /// Same with the satellite link root injected, so a test can point the
    /// hub at a fake satellite socket.
    pub(crate) async fn agent_request_deploy_via(
        &self,
        caller: Uuid,
        request: AgentRequestDeployRequestV1,
        service: &DeployService,
        now: DateTime<Utc>,
        satellite_root: &std::path::Path,
    ) -> Result<AgentRequestDeployReceiptV1> {
        {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(DEPLOY_NOT_AUTHORIZED.into()))?;
            if !projection
                .verbs
                .contains(&AgentControlVerbV1::RequestDeploy)
            {
                return Err(DaemonError::PolicyDenied(diagnose(&store, caller).into()));
            }
        }
        if let Some(peer) = request.peer_id {
            return self
                .request_satellite_deploy(caller, &request, peer.0, satellite_root)
                .await;
        }
        stage_owned_deploy(&self.store, caller, None, request, service, now).await
    }

    /// #1017 slice 2: ask the paired satellite to run its own deploy flow over
    /// the link. The hub stores nothing; the satellite's deploy row and
    /// `AgentGetDaemonInfo satellites` are the record.
    async fn request_satellite_deploy(
        &self,
        caller: Uuid,
        request: &AgentRequestDeployRequestV1,
        peer_id: Uuid,
        root: &std::path::Path,
    ) -> Result<AgentRequestDeployReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let refused = || DaemonError::PolicyDenied(SATELLITE_TARGET_NOT_AUTHORIZED.into());
        let (peer, wire) = {
            let store = self.store.lock().await;
            let view = store.satellite_registry_view()?;
            let value = view
                .peers
                .iter()
                .find(|value| value.config.peer_id.0 == peer_id)
                .ok_or_else(refused)?;
            let config = &value.config;
            if !(config.enabled
                && config.read_enabled
                && config.dispatch_enabled
                && config.expected_installation_id.is_some())
                || store.satellite_peer_scope(peer_id)?.is_empty()
            {
                return Err(refused());
            }
            let label = store
                .get_session(caller)
                .ok()
                .flatten()
                .map(|session| session.agent_role.clone().or(session.title.clone()))
                .unwrap_or_default()
                .unwrap_or_default();
            let wire = SatelliteDeployRequestV1 {
                wire_version: SATELLITE_WIRE_VERSION_V1,
                hub_installation_id: SatelliteUuidV1(store.satellite_installation_id()?),
                sender_label: crate::satellite::dispatch::sanitize_label(&label),
                sender_session_id: caller,
                sha: request.sha.clone(),
                binaries_dir: request.binaries_dir.clone().unwrap_or_default(),
                idempotency_key: request.idempotency_key.clone(),
                max_wait_secs: request.max_wait_secs,
            };
            (crate::satellite::hub::registry_peer(value), wire)
        };
        crate::satellite::deploy_link::request_hub_deploy(root, &peer, &wire).await
    }
}

/// #1131: the stable replay identity of a satellite hub deploy. The declared
/// scope roots are looked up for a replay; the resolved root keys a new row,
/// so an identical retry after the seat rotated returns the original deploy.
pub(crate) struct DeployReplayScope<'a> {
    pub(crate) lookup: &'a [Uuid],
    pub(crate) record: Uuid,
}

/// The satellite-independent core: replay, supervisor and budget gates, stage
/// and verify the binaries, and record the `staged` row owned by `owner` (the
/// session whose wake settles it). `scope` is `None` for an owner-keyed replay
/// (the hub) and the declared scope roots on a satellite.
/// Authority is the caller's job (the manager on the hub, the allowlisted hub
/// on a satellite).
pub(crate) async fn stage_owned_deploy(
    store: &Arc<Mutex<Store>>,
    owner: Uuid,
    scope: Option<DeployReplayScope<'_>>,
    request: AgentRequestDeployRequestV1,
    service: &DeployService,
    now: DateTime<Utc>,
) -> Result<AgentRequestDeployReceiptV1> {
    let live_schema = store.lock().await.schema_user_version()?;
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let dir = request.binaries_dir.clone().unwrap_or_default();
    let fingerprint = DeployService::fingerprint(&request.sha, &dir, request.wait_secs());
    {
        let store = store.lock().await;
        let scoped = match &scope {
            Some(scope) => store.replay_agent_deploy_scoped(
                scope.lookup,
                &request.idempotency_key,
                &fingerprint,
            )?,
            None => None,
        };
        // A row recorded before the scope existed is owner-keyed.
        let replay = match scoped {
            Some(row) => Some(row),
            None => store.replay_agent_deploy(owner, &request.idempotency_key, &fingerprint)?,
        };
        if let Some(row) = replay {
            return Ok(receipt(&row, true));
        }
        if !service.is_supervised() {
            return Err(DaemonError::PolicyDenied(DEPLOY_NEEDS_SUPERVISOR.into()));
        }
        service.check_target()?;
        service.check_budget(&store, now)?;
    }
    let id = Uuid::new_v4();
    // Copying and hashing binaries runs without the store lock.
    let staged = {
        let dir = dir.clone();
        let sha = request.sha.clone();
        let plan = service.stage_plan();
        tokio::task::spawn_blocking(move || plan.stage_binaries(id, &dir, &sha, live_schema))
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))??
    };
    let store = store.lock().await;
    let inserted = store.insert_agent_deploy_scoped(
        &NewDeploy {
            id,
            owner_session_id: owner,
            idempotency_key: &request.idempotency_key,
            sha: &request.sha,
            fingerprint,
            manifest: &staged,
            max_wait_secs: request.wait_secs(),
        },
        scope.as_ref().map(|scope| scope.record),
        now,
    );
    match inserted {
        Ok(row) => Ok(receipt(&row, false)),
        Err(error) => {
            remove_staged(&staged, id);
            Err(error)
        }
    }
}

/// Which precondition a manager caller is missing, without revealing more than
/// the caller's own policy.
fn diagnose(store: &crate::store::Store, caller: Uuid) -> &'static str {
    let Ok((config, true)) = store.manager_config_for_caller(caller) else {
        return DEPLOY_NOT_AUTHORIZED;
    };
    match store.get_harness_manager_policy(config.project_id) {
        Ok(Some(grant)) if !grant.revoked => {
            if grant
                .policy
                .capabilities
                .contains(&ManagerCapabilityV2::Deploy)
            {
                DEPLOY_EXECUTE_REQUIRED
            } else {
                DEPLOY_CAPABILITY_REQUIRED
            }
        }
        _ => DEPLOY_NOT_AUTHORIZED,
    }
}
