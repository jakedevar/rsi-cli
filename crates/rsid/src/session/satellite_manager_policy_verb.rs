//! Hub admission for bounded satellite policy pushes (#1748).
use super::{AgentControlHandle, authorize_deploy};
use crate::error::{DaemonError, Result};
use crate::store::harness_manager_v2::ManagerCallerV1;
use rsi_common::portfolio_nodes::ManagerResourceCapsV1;
use rsi_common::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteUuidV1};
use rsi_common::satellite_dispatch::SATELLITE_TARGET_NOT_AUTHORIZED;
use rsi_common::satellite_manager_policy::*;
use uuid::Uuid;

fn denied() -> DaemonError {
    DaemonError::PolicyDenied(SATELLITE_TARGET_NOT_AUTHORIZED.into())
}
/// The hub caller's effective launch list and resource caps for the project it
/// pushes to: Deploy authority first, then the live grant (a portfolio seat's
/// includes every parent ceiling). Every failure is `target_not_authorized`.
fn hub_effective_grant(
    store: &crate::store::Store,
    caller: Uuid,
    project_id: Option<Uuid>,
) -> Result<(
    Vec<rsi_common::harness_manager_v2::ManagerLaunchChoiceV2>,
    ManagerResourceCapsV1,
)> {
    authorize_deploy(store, caller, project_id).map_err(|_| denied())?;
    let (config, grant, global) = match store
        .resolve_manager_caller(caller, project_id)
        .map_err(|_| denied())?
    {
        ManagerCallerV1::Legacy {
            config,
            is_manager: true,
        } => {
            let grant = store
                .manager_policy_for_config(&config)?
                .filter(|g| !g.revoked)
                .ok_or_else(denied)?;
            (config, grant, false)
        }
        ManagerCallerV1::Global(a) => (a.config, a.grant, true),
        _ => return Err(denied()),
    };
    let mut launches = grant.policy.allowed_launches.clone();
    let mut caps = ManagerResourceCapsV1::from_policy(&grant.policy);
    if global {
        // Include live parent grants; a later parent narrowing applies at
        // this call, rather than relying on the child's old saved policy.
        let mut node = store.portfolio_node_for_seat(caller)?.ok_or_else(denied)?;
        for _ in 0..64 {
            let Some(parent) = node.parent_node_id else {
                break;
            };
            node = store
                .get_portfolio_node(parent)?
                .filter(|n| n.state == "active")
                .ok_or_else(denied)?;
            let parent_launches = rsi_common::grant_narrowing::portfolio_effective_launches(
                &node.grant.allowed_launches,
                &node.grant.project_policy,
            );
            launches = rsi_common::grant_narrowing::intersect_launches(&launches, &parent_launches);
            caps.intersect(&node.grant.project_policy);
        }
        if node.parent_node_id.is_some() {
            return Err(denied());
        }
    } else {
        launches = store.manager_effective_launches(&config, &launches)?;
        // #3417: a provider grant's any-model entry is charged against the
        // hub's own spend; a satellite policy carries exact tuples only.
        launches.retain(|launch| !launch.is_provider_wide());
        caps = store
            .project_effective_resource_caps(config.project_id)?
            .ok_or_else(denied)?;
    }
    Ok((launches, caps))
}

impl AgentControlHandle {
    pub async fn agent_push_satellite_manager_policy(
        &self,
        caller: Uuid,
        request: AgentPushSatelliteManagerPolicyRequestV1,
    ) -> Result<SatelliteManagerPolicyReceiptV1> {
        let root = rsi_common::identity::data_path("satellites", "satellites");
        self.push_satellite_manager_policy_via(caller, request, &root)
            .await
    }
    pub(crate) async fn push_satellite_manager_policy_via(
        &self,
        caller: Uuid,
        request: AgentPushSatelliteManagerPolicyRequestV1,
        root: &std::path::Path,
    ) -> Result<SatelliteManagerPolicyReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        // Snapshot admission and the paired link under the store lock, as for
        // deploy. A slow satellite must not stall unrelated hub operations.
        let store = self.store.lock().await;
        let (launches, caps) = hub_effective_grant(&store, caller, request.project_id)?;
        let view = store.satellite_registry_view()?;
        let value = view
            .peers
            .iter()
            .find(|p| p.config.peer_id == request.peer_id)
            .ok_or_else(denied)?;
        let peer = &value.config;
        if !(peer.enabled
            && peer.read_enabled
            && peer.dispatch_enabled
            && peer.expected_installation_id.is_some())
            // #1928: the same hub-side scope check as message queueing; a
            // declared project id is verified here, resolved by the satellite.
            || !store.hub_peer_declares_address(peer.peer_id.0, request.remote_session_id.0)?
        {
            return Err(denied());
        }
        if !request.policy.within(&launches, &caps) {
            return Err(DaemonError::PolicyDenied(POLICY_BOUNDS.into()));
        }
        let wire = SatelliteManagerPolicyRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(store.satellite_installation_id()?),
            sender_session_id: caller,
            request,
        };
        let peer = crate::satellite::hub::registry_peer(value);
        drop(store);
        crate::satellite::deploy_link::push_manager_policy(root, &peer, &wire).await
    }

    /// #2750: ask a paired satellite to move its failed manager seat to
    /// another provider. Deploy authority, the same paired link and the same
    /// declared-address check as a policy push; the requested provider, model
    /// and effort must be inside the hub's own effective launch grant.
    pub async fn agent_reseat_satellite_manager(
        &self,
        caller: Uuid,
        request: rsi_common::manager_reseat::AgentReseatSatelliteManagerRequestV1,
    ) -> Result<rsi_common::manager_reseat::SatelliteManagerReseatReceiptV1> {
        let root = rsi_common::identity::data_path("satellites", "satellites");
        self.reseat_satellite_manager_via(caller, request, &root)
            .await
    }

    pub(crate) async fn reseat_satellite_manager_via(
        &self,
        caller: Uuid,
        request: rsi_common::manager_reseat::AgentReseatSatelliteManagerRequestV1,
        root: &std::path::Path,
    ) -> Result<rsi_common::manager_reseat::SatelliteManagerReseatReceiptV1> {
        use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
        use rsi_common::manager_reseat::*;
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let store = self.store.lock().await;
        let (launches, _caps) = hub_effective_grant(&store, caller, request.project_id)?;
        let view = store.satellite_registry_view()?;
        let value = view
            .peers
            .iter()
            .find(|p| p.config.peer_id == request.peer_id)
            .ok_or_else(denied)?;
        let peer = &value.config;
        if !(peer.enabled
            && peer.read_enabled
            && peer.dispatch_enabled
            && peer.expected_installation_id.is_some())
            || !store.hub_peer_declares_address(peer.peer_id.0, request.remote_session_id.0)?
        {
            return Err(denied());
        }
        // A reseat launches a session: it never widens the hub's own grant.
        // A model left to the project default cannot be checked against a
        // grant, and `ManagerLaunchChoiceV2` has no any-model entry (its
        // `model` is a required exact id; only effort can be omitted), so an
        // omitted model is refused rather than passed on any entry for the
        // provider. A given model needs an entry that allows its exact
        // provider, model and effort (`ManagerLaunchChoiceV2::allows`).
        let within = request.model.as_ref().is_some_and(|model| {
            let wanted = ManagerLaunchChoiceV2 {
                provider: request.provider,
                model: model.clone(),
                effort: request.effort.clone(),
            };
            launches.iter().any(|entry| entry.allows(&wanted))
        });
        if !within {
            return Err(DaemonError::PolicyDenied(SATELLITE_RESEAT_BOUNDS.into()));
        }
        let wire = SatelliteManagerReseatRequestV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(store.satellite_installation_id()?),
            sender_session_id: caller,
            request,
        };
        let peer = crate::satellite::hub::registry_peer(value);
        drop(store);
        crate::satellite::deploy_link::reseat_manager(root, &peer, &wire).await
    }
}
