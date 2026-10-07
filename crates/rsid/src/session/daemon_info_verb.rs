//! `AgentGetDaemonInfo` (#1045 slice 1): the hub daemon's build SHA, binary
//! hash, start time, schema version, disk free and load, for the current
//! appointed manager or the current lead of the caller's Epic.
//!
//! Authority is the same projection `AgentGetProviderStatus` uses; workers are
//! refused `daemon_info_not_authorized`. The result carries no environment
//! value and no credential.

use super::agent_verbs::AgentControlHandle;
use crate::daemon_info::DaemonInfoService;
use crate::error::{DaemonError, Result};
use rsi_common::agent_daemon_info::AgentGetDaemonInfoResultV1;
use uuid::Uuid;

/// Stable refusal for a caller that is neither manager nor Epic lead.
pub const DAEMON_INFO_NOT_AUTHORIZED: &str = "daemon_info_not_authorized";

impl AgentControlHandle {
    /// # Errors
    /// `daemon_info_not_authorized`, or a persistence error.
    pub async fn agent_get_daemon_info(&self, caller: Uuid) -> Result<AgentGetDaemonInfoResultV1> {
        self.agent_get_daemon_info_with(caller, DaemonInfoService::global())
            .await
    }

    /// Same with the service injected, so a test can pin the binary and paths.
    pub(crate) async fn agent_get_daemon_info_with(
        &self,
        caller: Uuid,
        service: &DaemonInfoService,
    ) -> Result<AgentGetDaemonInfoResultV1> {
        let root = rsi_common::identity::data_path("satellites", "satellites");
        self.agent_get_daemon_info_via(caller, service, &root).await
    }

    /// Same with the satellite link root injected.
    pub(crate) async fn agent_get_daemon_info_via(
        &self,
        caller: Uuid,
        service: &DaemonInfoService,
        satellite_root: &std::path::Path,
    ) -> Result<AgentGetDaemonInfoResultV1> {
        // Listing the held creates reads the store, so only while the hold is
        // in force (#1417).
        let list_held_creates = self
            .host_load
            .as_ref()
            .is_some_and(|gate| gate.hold_now().is_some());
        let (schema_version, peers, queued_creates) = {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(DAEMON_INFO_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager) {
                return Err(DaemonError::PolicyDenied(DAEMON_INFO_NOT_AUTHORIZED.into()));
            }
            // Satellites are the appointed manager's to see; an Epic lead gets
            // the hub only.
            let peers = if projection.is_manager {
                store
                    .satellite_registry_view()?
                    .peers
                    .iter()
                    .filter(|value| {
                        value.config.enabled
                            && value.config.read_enabled
                            && value.config.expected_installation_id.is_some()
                    })
                    .map(crate::satellite::hub::registry_peer)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let queued_creates = if list_held_creates {
                store.queued_manager_create_sessions(crate::host_load::HELD_LIST_LIMIT)?
            } else {
                Vec::new()
            };
            (store.schema_user_version()?, peers, queued_creates)
        };
        // The store lock is released before hashing, statvfs, /proc reads or
        // any link I/O. Peers are read concurrently, each bounded by the link
        // deadline, so one dark satellite cannot stall the call.
        let satellites = futures::future::join_all(peers.iter().map(|peer| {
            crate::satellite::deploy_link::read_peer_daemon_info(satellite_root, peer)
        }))
        .await;
        Ok(AgentGetDaemonInfoResultV1 {
            hub: service.snapshot(schema_version),
            satellites,
            deploy_drain: self
                .deploy_drain
                .as_ref()
                .map(|drain| drain.status())
                .unwrap_or_default(),
            host_load: self
                .host_load
                .as_ref()
                .map(|gate| gate.status(&queued_creates))
                .unwrap_or_default(),
        })
    }
}
