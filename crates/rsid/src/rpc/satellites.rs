use super::*;

impl RpcServer {
    pub(super) async fn handle_get_satellite_identity(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "GetSatelliteIdentity takes no parameters".into(),
            ));
        }
        let (installation_id, health) = {
            let store = self.session_manager.store().lock().await;
            let installation_id = store.satellite_installation_id()?;
            // Health is additive: a failed read must not hide the identity.
            (installation_id, crate::satellite::health(&store).ok())
        };
        let mut identity =
            crate::satellite::identity(installation_id, self.satellite_incarnation_id);
        identity.health = health;
        Ok(serde_json::to_value(identity)?)
    }

    pub(super) async fn handle_list_satellite_sessions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite::{SATELLITE_MAX_CURSOR_BYTES, SatelliteSessionPageRequestV1};
        let object = request.params.as_object().ok_or_else(|| {
            DaemonError::InvalidParam("satellite request must be an object".into())
        })?;
        if object.len() > 3
            || object
                .keys()
                .any(|key| !matches!(key.as_str(), "wire_version" | "limit" | "cursor"))
            || serde_json::to_vec(&request.params)?.len()
                > usize::from(SATELLITE_MAX_CURSOR_BYTES) + 128
        {
            return Err(DaemonError::InvalidParam(
                "malformed or oversized satellite request".into(),
            ));
        }
        let params: SatelliteSessionPageRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| {
                DaemonError::InvalidParam(format!("invalid satellite request: {error}"))
            })?;
        params
            .validate(&rsi_common::satellite::SatelliteReadLimitsV1::default())
            .map_err(|error| DaemonError::InvalidParam(error.to_string()))?;
        let installation_id = self
            .session_manager
            .store()
            .lock()
            .await
            .satellite_installation_id()?;
        let sessions = if params.cursor.is_none() {
            Some(self.session_manager.list_sessions().await)
        } else {
            None
        };
        let page = self
            .satellite_snapshots
            .page(
                params,
                sessions,
                installation_id,
                self.satellite_incarnation_id,
            )
            .await?;
        Ok(serde_json::to_value(page)?)
    }

    pub(super) async fn handle_list_satellite_peers(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "ListSatellitePeers takes no parameters".into(),
            ));
        }
        let view = self
            .session_manager
            .store()
            .lock()
            .await
            .satellite_registry_view()?;
        Ok(serde_json::to_value(view)?)
    }

    pub(super) async fn handle_put_satellite_peer(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite::SatellitePutPeerRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 2_048 {
            return Err(DaemonError::InvalidParam(
                "satellite peer request exceeds byte limit".into(),
            ));
        }
        let input: SatellitePutPeerRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| {
                DaemonError::InvalidParam(format!("invalid satellite peer: {error}"))
            })?;
        let root = rsi_common::identity::data_path("satellites", "satellites");
        let revision = self
            .session_manager
            .store()
            .lock()
            .await
            .put_satellite_peer(&input, &root)?;
        Ok(serde_json::json!({ "revision": revision }))
    }

    pub(super) async fn handle_put_satellite_peer_scope(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite_dispatch::SatellitePutScopeRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 8_192 {
            return Err(DaemonError::InvalidParam(
                "satellite scope request exceeds byte limit".into(),
            ));
        }
        let input: SatellitePutScopeRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| {
                DaemonError::InvalidParam(format!("invalid satellite scope: {error}"))
            })?;
        let revision = self
            .session_manager
            .store()
            .lock()
            .await
            .put_satellite_peer_scope(
                input.expected_registry_revision,
                input.peer_id.0,
                &input.remote_session_ids,
            )?;
        Ok(serde_json::json!({ "revision": revision }))
    }

    pub(super) async fn handle_get_satellite_inbound_policy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "GetSatelliteInboundPolicy takes no parameters".into(),
            ));
        }
        let policy = self
            .session_manager
            .store()
            .lock()
            .await
            .satellite_inbound_policy()?;
        Ok(serde_json::to_value(policy)?)
    }

    pub(super) async fn handle_put_satellite_inbound_policy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite_dispatch::PutSatelliteInboundPolicyRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 8_192 {
            return Err(DaemonError::InvalidParam(
                "satellite inbound policy exceeds byte limit".into(),
            ));
        }
        let input: PutSatelliteInboundPolicyRequestV1 =
            serde_json::from_value(request.params.clone()).map_err(|error| {
                DaemonError::InvalidParam(format!("invalid satellite inbound policy: {error}"))
            })?;
        self.session_manager
            .store()
            .lock()
            .await
            .put_satellite_inbound_policy(&input.policy)?;
        Ok(serde_json::to_value(input.policy)?)
    }

    pub(super) async fn handle_deliver_hub_message(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite_dispatch::{
            SATELLITE_MESSAGE_MAX_BYTES, SatelliteDeliverRequestV1,
        };
        if serde_json::to_vec(&request.params)?.len() > SATELLITE_MESSAGE_MAX_BYTES * 2 + 1_024 {
            return Err(DaemonError::InvalidParam(
                "hub delivery exceeds byte limit".into(),
            ));
        }
        let input: SatelliteDeliverRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| DaemonError::InvalidParam(format!("invalid hub delivery: {error}")))?;
        let result =
            crate::satellite::delivery::deliver_hub_message(&self.session_manager, input).await?;
        Ok(serde_json::to_value(result)?)
    }

    /// #1103: the allowlisted hub pulls the reports the satellite's manager
    /// queued. Operator-only wire method (never in the agent catalog); the
    /// satellite enforces its own inbound policy.
    pub(super) async fn handle_fetch_hub_reports(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite_dispatch::SatelliteFetchReportsRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 8_192 {
            return Err(DaemonError::InvalidParam(
                "hub report fetch exceeds byte limit".into(),
            ));
        }
        let input: SatelliteFetchReportsRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|_| DaemonError::InvalidParam("satellite_report_invalid".into()))?;
        input
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let reply = self
            .session_manager
            .store()
            .lock()
            .await
            .take_hub_reports(input.hub_installation_id.0, &input.acked)?;
        Ok(serde_json::to_value(reply)?)
    }

    /// #1017 slice 2: hub-initiated deploy on this satellite. Operator-only
    /// wire method (never in the agent catalog); the satellite enforces its own
    /// inbound policy.
    pub(super) async fn handle_request_hub_deploy(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite_dispatch::SatelliteDeployRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 4_096 {
            return Err(DaemonError::InvalidParam(
                "hub deploy exceeds byte limit".into(),
            ));
        }
        let input: SatelliteDeployRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| DaemonError::InvalidParam(format!("invalid hub deploy: {error}")))?;
        let receipt = crate::satellite::delivery::request_hub_deploy(
            &self.session_manager,
            crate::deploy::DeployService::global(),
            input,
            chrono::Utc::now(),
        )
        .await?;
        Ok(serde_json::to_value(receipt)?)
    }

    pub(super) async fn handle_put_satellite_link(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite::SatellitePutLinkRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 2_048 {
            return Err(DaemonError::InvalidParam(
                "satellite link request exceeds byte limit".into(),
            ));
        }
        let input: SatellitePutLinkRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| {
                DaemonError::InvalidParam(format!("invalid satellite link: {error}"))
            })?;
        let root = rsi_common::identity::data_path("satellites", "satellites");
        let revision = self
            .session_manager
            .store()
            .lock()
            .await
            .put_satellite_link(&input, &root)?;
        Ok(serde_json::json!({ "revision": revision }))
    }

    pub(super) async fn handle_probe_satellite_link(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use crate::satellite::registry::{LinkDirection, RegistryLink};
        use rsi_common::satellite::{
            SatelliteLinkDirectionV1, SatelliteProbeLinkRequestV1, SatelliteProbeLinkResultV1,
        };
        if serde_json::to_vec(&request.params)?.len() > 256 {
            return Err(DaemonError::InvalidParam(
                "satellite probe request exceeds byte limit".into(),
            ));
        }
        let input: SatelliteProbeLinkRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| {
                DaemonError::InvalidParam(format!("invalid satellite probe: {error}"))
            })?;
        let view = self
            .session_manager
            .store()
            .lock()
            .await
            .satellite_registry_view()?;
        let peer = view
            .peers
            .into_iter()
            .find(|peer| peer.config.peer_id == input.peer_id)
            .ok_or_else(|| DaemonError::InvalidParam("satellite peer does not exist".into()))?;
        let link = peer
            .links
            .into_iter()
            .find(|link| link.link_id == input.link_id)
            .ok_or_else(|| DaemonError::InvalidParam("satellite link does not exist".into()))?;
        if !peer.config.enabled || !link.enabled {
            return Err(DaemonError::PolicyDenied(
                "satellite peer or link is disabled".into(),
            ));
        }
        let root = rsi_common::identity::data_path("satellites", "satellites");
        let registry_link = RegistryLink {
            id: link.link_id.0,
            direction: match link.direction {
                SatelliteLinkDirectionV1::DialHomeReverse => LinkDirection::DialHomeReverse,
                SatelliteLinkDirectionV1::DirectLocalForward => LinkDirection::DirectLocalForward,
            },
            socket_path: link.socket_path.into(),
            ssh_target: link.ssh_target,
            trust_reference: link.trust_reference.clone(),
            enabled: true,
            priority: link.priority,
        };
        let identity = crate::satellite::poll::inspect_link_identity(&root, &registry_link)
            .await
            .map_err(|error| {
                DaemonError::Rpc(format!("satellite identity probe failed: {error}"))
            })?;
        let matches_expected = peer
            .config
            .expected_installation_id
            .map(|expected| expected == identity.installation_id);
        if matches_expected == Some(false) {
            self.session_manager
                .store()
                .lock()
                .await
                .record_satellite_failure(
                    input.peer_id.0,
                    crate::store::satellite_registry::SatelliteFailure::Insecure,
                )?;
        }
        Ok(serde_json::to_value(SatelliteProbeLinkResultV1 {
            peer_id: input.peer_id,
            link_id: input.link_id,
            direction: link.direction,
            trust_reference: link.trust_reference,
            identity,
            matches_expected,
        })?)
    }

    pub(super) async fn handle_list_hub_satellite_sessions(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        use rsi_common::satellite::SatelliteHubSessionsRequestV1;
        if serde_json::to_vec(&request.params)?.len() > 256 {
            return Err(DaemonError::InvalidParam(
                "satellite cache request exceeds byte limit".into(),
            ));
        }
        let input: SatelliteHubSessionsRequestV1 = serde_json::from_value(request.params.clone())
            .map_err(|error| {
            DaemonError::InvalidParam(format!("invalid satellite cache request: {error}"))
        })?;
        let page = self
            .session_manager
            .store()
            .lock()
            .await
            .satellite_cached_sessions_page(&input)?;
        Ok(serde_json::to_value(page)?)
    }
}
