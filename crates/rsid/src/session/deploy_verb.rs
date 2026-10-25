//! `AgentRequestDeploy` (#1045 slice 2): accept one deploy request from the
//! current appointed manager holding the operator-granted `Deploy` capability
//! (Execute mode, not paused), stage and verify the binaries, and record a
//! `staged` row the daemon's deploy runner drives to a quiet-point restart.

use super::agent_verbs::AgentControlHandle;
use crate::deploy::{DeployService, remove_staged};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::agent_deploys::{DeployInterrupt, DeployRow, NewDeploy};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::agent_deploy::{
    AgentRequestDeployReceiptV1, AgentRequestDeployRequestV1, DEPLOY_CANCELLED,
    DEPLOY_CAPABILITY_REQUIRED, DEPLOY_EXECUTE_REQUIRED, DEPLOY_NEEDS_SUPERVISOR,
    DEPLOY_NOT_AUTHORIZED, DeployState, skipped_binaries,
};
use rsi_common::harness_manager_v2::ManagerCapabilityV2;
use rsi_common::rolling_queue::DeployQaWarningV1;
use rsi_common::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteUuidV1};
use rsi_common::satellite_dispatch::{SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeployRequestV1};
use std::sync::Arc;
use uuid::Uuid;

fn receipt(
    row: &DeployRow,
    interrupt: DeployInterrupt,
    replayed: bool,
) -> AgentRequestDeployReceiptV1 {
    AgentRequestDeployReceiptV1 {
        interrupt_workers: interrupt.requested,
        interrupted_workers: interrupt.interrupted,
        expected_interrupted_workers: Vec::new(),
        expected_requeued_jobs: Vec::new(),
        requeued_jobs: Vec::new(),
        expected_surviving_jobs: Vec::new(),
        surviving_jobs: Vec::new(),
        job_grace_secs: None,
        cancelled_jobs: interrupt.cancelled_jobs,
        deploy_id: row.id,
        state: row.state,
        sha: row.sha.clone(),
        deadline_at: row.deadline_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        binaries: row.manifest.clone(),
        skipped: skipped_binaries(&row.manifest),
        replayed,
        reason: row.reason.clone(),
        blockers: Vec::new(),
        waiting_since: None,
        qa_warnings: Vec::new(),
    }
}

/// #2056: the receipt of a deploy that may still be waiting, with what holds
/// its quiet point read live (the deploy owner's way to ask "why staged?").
pub(crate) fn receipt_with_blockers(
    store: &Store,
    row: &DeployRow,
    interrupt: DeployInterrupt,
    replayed: bool,
) -> Result<AgentRequestDeployReceiptV1> {
    let mut receipt = receipt(row, interrupt, replayed);
    for id in &receipt.cancelled_jobs {
        if store.agent_job_requeued_by_deploy(*id)? == Some(row.id) {
            receipt.requeued_jobs.push(*id);
        }
    }
    // #3554: the jobs a restart left running, as the restarting poll marked them.
    receipt.surviving_jobs = store.deploy_surviving_jobs(row.id)?;
    if row.state == DeployState::Staged
        && let Some(owner) = row.owner_session_id
    {
        // #3554: a running ordinary test job of an `interrupt_workers` deploy
        // keeps running across the restart and is no blocker.
        let surviving = crate::agent_jobs::deploy_surviving_jobs_or_none(store, row);
        receipt.blockers = store.deploy_quiet_blocker_details_surviving(owner, true, &surviving)?;
        if receipt.interrupt_workers {
            receipt.expected_interrupted_workers = store.deploy_mid_turn_workers(owner)?;
            receipt.expected_surviving_jobs = surviving;
        }
        receipt.waiting_since = store
            .agent_deploy_created_at(row.id)?
            .map(|at| at.to_rfc3339_opts(SecondsFormat::Nanos, true));
    }
    Ok(receipt)
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
        let admitted = {
            let store = self.store.lock().await;
            authorize_deploy(&store, caller, request.project_id)?
        };
        if request.is_cancel() && request.peer_id.is_none() {
            return self
                .cancel_owned_deploy(caller, &request, now, false, None)
                .await;
        }
        if let Some(peer) = request.peer_id {
            let (sha, project, cancel) =
                (request.sha.clone(), request.project_id, request.is_cancel());
            let mut receipt = self
                .request_satellite_deploy(caller, &request, peer.0, satellite_root)
                .await?;
            // #2772: a satellite deploy of a SHA the hub's latest full-library
            // green does not cover warns exactly like a hub deploy; a cancel
            // deploys nothing and warns of nothing.
            if !cancel {
                receipt.qa_warnings = self.deploy_qa_warnings(caller, project, &sha, now).await;
            }
            return Ok(receipt);
        }
        let authority = DeployAuthority {
            caller,
            project_id: request.project_id,
            fence: admitted,
        };
        let (sha, project) = (request.sha.clone(), request.project_id);
        let mut receipt = stage_owned_deploy(
            &self.store,
            caller,
            None,
            request,
            service,
            now,
            Some(&authority),
            self.deploy_drain
                .as_deref()
                .zip(self.runtime_config.as_deref()),
        )
        .await?;
        receipt.qa_warnings = self.deploy_qa_warnings(caller, project, &sha, now).await;
        Ok(receipt)
    }

    /// #2743: a deploy of a SHA that the latest full-library-green SHA does
    /// not cover is accepted with a `not_qa_green` warning that names the
    /// project's open `qa` Issues; it is never refused, and never silent. A
    /// failed read of the health is logged and adds nothing: the deploy is the
    /// manager's call, the warning is information.
    async fn deploy_qa_warnings(
        &self,
        caller: Uuid,
        project: Option<Uuid>,
        sha: &str,
        now: DateTime<Utc>,
    ) -> Vec<DeployQaWarningV1> {
        let (health, repo) = {
            let store = self.store.lock().await;
            let project = project.or_else(|| {
                store
                    .get_session(caller)
                    .ok()
                    .flatten()
                    .and_then(|session| session.project_id)
            });
            let Some(project) = project else {
                return Vec::new();
            };
            let health = match store.rolling_health_for_project(project, now) {
                Ok(health) => health,
                Err(error) => {
                    tracing::warn!(%project, %error, "deploy qa warning unavailable");
                    return Vec::new();
                }
            };
            let repo = store
                .get_project(project)
                .ok()
                .flatten()
                .and_then(|project| project.path);
            (health, repo)
        };
        let ancestry = {
            let sha = sha.to_string();
            move |green: &str| {
                repo.as_deref().is_some_and(|repo| {
                    crate::rolling_queue::source_descends_from(
                        std::path::Path::new(repo),
                        &sha,
                        green,
                    )
                })
            }
        };
        // `git` runs outside the store lock (above) and off the async threads.
        let sha = sha.to_string();
        tokio::task::spawn_blocking(move || {
            DeployQaWarningV1::for_deploy(&sha, &health, ancestry)
                .into_iter()
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// #1320/#1311: cancel the caller's own waiting deploy. It settles
    /// `failed` with `deploy_cancelled`, its staged copies are removed and its
    /// hold on new launches ends at once.
    pub(crate) async fn cancel_owned_deploy(
        &self,
        caller: Uuid,
        request: &AgentRequestDeployRequestV1,
        now: DateTime<Utc>,
        require_waiting: bool,
        scope: Option<&[Uuid]>,
    ) -> Result<AgentRequestDeployReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let (row, interrupt) = {
            let store = self.store.lock().await;
            let row = if let Some(scope) = scope {
                store.cancel_scoped_agent_deploy_if_waiting(
                    scope,
                    &request.idempotency_key,
                    &request.sha,
                    now,
                )?
            } else if require_waiting {
                store.cancel_agent_deploy_if_waiting(
                    caller,
                    &request.idempotency_key,
                    &request.sha,
                    now,
                )?
            } else {
                store.cancel_agent_deploy(caller, &request.idempotency_key, &request.sha, now)?
            };
            let interrupt = store.agent_deploy_interrupt(row.id)?;
            (row, interrupt)
        };
        if row.reason.as_deref() == Some(DEPLOY_CANCELLED) {
            remove_staged(&row.manifest, row.id);
            if let Some(drain) = &self.deploy_drain {
                drain.release(row.id);
            }
        }
        let store = self.store.lock().await;
        receipt_with_blockers(&store, &row, interrupt, false)
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
                || !store.hub_peer_declares_scope(peer_id)?
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
                // An explicit `false` asks for nothing: send no field, so an
                // older satellite that denies unknown fields keeps working.
                interrupt_workers: request.interrupts_workers().then_some(true),
                cancel: request.cancel,
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
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    owner: Uuid,
    scope: Option<DeployReplayScope<'_>>,
    request: AgentRequestDeployRequestV1,
    service: &DeployService,
    now: DateTime<Utc>,
    authority: Option<&DeployAuthority>,
    hold: Option<(
        &crate::deploy_drain::DeployDrain,
        &crate::config::RuntimeConfig,
    )>,
) -> Result<AgentRequestDeployReceiptV1> {
    let live_schema = store.lock().await.schema_user_version()?;
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let dir = request.binaries_dir.clone().unwrap_or_default();
    let interrupt_workers = request.interrupts_workers();
    let fingerprint =
        DeployService::fingerprint(&request.sha, &dir, request.wait_secs(), interrupt_workers);
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
            let interrupt = store.agent_deploy_interrupt(row.id)?;
            return receipt_with_blockers(&store, &row, interrupt, true);
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
    #[cfg(test)]
    if let Some(authority) = authority {
        super::effect_fence::seam::run(store, authority.caller, "stage_deploy").await;
    }
    let store = store.lock().await;
    // #1279: staging ran without the lock. Re-resolve the manager or global
    // deploy authority under the lock that stays held through the insert, and
    // refuse (removing what was staged) if it changed.
    if let Some(authority) = authority {
        let rechecked =
            authorize_deploy(&store, authority.caller, authority.project_id).and_then(|current| {
                super::effect_fence::require_unchanged(
                    &authority.fence,
                    &current,
                    DEPLOY_NOT_AUTHORIZED,
                )
            });
        if let Err(error) = rechecked {
            remove_staged(&staged, id);
            return Err(error);
        }
    }
    let inserted = store.insert_agent_deploy_scoped(
        &NewDeploy {
            id,
            owner_session_id: owner,
            idempotency_key: &request.idempotency_key,
            sha: &request.sha,
            fingerprint,
            manifest: &staged,
            max_wait_secs: request.wait_secs(),
            interrupt_workers,
        },
        scope.as_ref().map(|scope| scope.record),
        now,
    );
    match inserted {
        Ok(row) => {
            if let Some((drain, config)) = hold {
                crate::deploy::engage_hold_at_stage(&store, &row, drain, config, now)?;
            }
            receipt_with_blockers(
                &store,
                &row,
                DeployInterrupt {
                    requested: interrupt_workers,
                    interrupted: Vec::new(),
                    cancelled_jobs: Vec::new(),
                },
                false,
            )
        }
        Err(error) => {
            remove_staged(&staged, id);
            Err(error)
        }
    }
}

/// The authority a hub deploy was admitted under, replayed at the insert.
pub(crate) struct DeployAuthority {
    pub(crate) caller: Uuid,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) fence: String,
}

/// Admission of one hub deploy (the manager arm or the global seat's arm) and
/// the authority fence it was resolved under. Run again at the effect.
fn authorize_deploy(store: &Store, caller: Uuid, project_id: Option<Uuid>) -> Result<String> {
    let projection = store
        .agent_authority_projection(caller)
        .map_err(|_| DaemonError::PolicyDenied(DEPLOY_NOT_AUTHORIZED.into()))?;
    let own = store
        .get_session(caller)?
        .and_then(|session| session.project_id);
    let own_target = project_id.is_none_or(|project| Some(project) == own);
    let manager_deploy = projection.is_manager
        && own_target
        && projection
            .verbs
            .contains(&AgentControlVerbV1::RequestDeploy);
    if !manager_deploy {
        global_deploy_authorized(store, caller, project_id, &projection)?;
    }
    store
        .agent_authority_fence(caller, None)
        .map_err(|_| DaemonError::PolicyDenied(DEPLOY_NOT_AUTHORIZED.into()))
}

/// #1235: the global seat deploys for a granted project whose project policy
/// holds `Deploy` in Execute mode, unpaused. Any other caller keeps the
/// manager diagnosis; a project outside every arm is
/// `manager_project_not_in_scope`.
fn global_deploy_authorized(
    store: &crate::store::Store,
    caller: Uuid,
    project: Option<Uuid>,
    projection: &crate::store::agent_authority::AgentAuthorityProjection,
) -> Result<()> {
    use crate::store::harness_manager_v2::ManagerCallerV1;
    use rsi_common::harness_manager_v2::ManagerOperatingModeV2;
    if !projection.is_global_manager && project.is_none() {
        return Err(DaemonError::PolicyDenied(diagnose(store, caller).into()));
    }
    match store.resolve_manager_caller(caller, project) {
        Ok(ManagerCallerV1::Global(authority)) => {
            let policy = &authority.grant.policy;
            if !policy.capabilities.contains(&ManagerCapabilityV2::Deploy) {
                Err(DaemonError::PolicyDenied(DEPLOY_CAPABILITY_REQUIRED.into()))
            } else if policy.mode != ManagerOperatingModeV2::Execute || policy.paused {
                Err(DaemonError::PolicyDenied(DEPLOY_EXECUTE_REQUIRED.into()))
            } else {
                Ok(())
            }
        }
        Err(DaemonError::InvalidParam(code))
            if code == rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE =>
        {
            Err(DaemonError::InvalidParam(code))
        }
        _ => Err(DaemonError::PolicyDenied(diagnose(store, caller).into())),
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

#[path = "satellite_manager_policy_verb.rs"]
mod satellite_manager_policy_verb;
