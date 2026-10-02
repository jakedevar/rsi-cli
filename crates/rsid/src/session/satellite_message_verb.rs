//! `AgentSendSatelliteMessage` (#1017 slice 3): queue one message for a
//! scoped session on a paired satellite host.
//!
//! Authority is the current appointed manager only, resolved from the
//! token-bound caller. Every refusal (not the manager, unknown peer, target
//! out of scope, dispatch off) is the same `satellite_target_not_authorized`
//! so the reply reveals nothing about the remote host.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::satellite_dispatch::{
    AgentSendSatelliteMessageReceiptV1, AgentSendSatelliteMessageRequestV1,
    SATELLITE_TARGET_NOT_AUTHORIZED,
};
use uuid::Uuid;

impl AgentControlHandle {
    /// # Errors
    /// A stable `satellite_*` refusal, or a persistence error.
    pub async fn agent_send_satellite_message(
        &self,
        caller: Uuid,
        request: AgentSendSatelliteMessageRequestV1,
    ) -> Result<AgentSendSatelliteMessageReceiptV1> {
        let store = self.store.lock().await;
        let is_manager = store
            .agent_authority_projection(caller)
            .map(|projection| projection.is_manager)
            .unwrap_or(false);
        if !is_manager {
            return Err(DaemonError::PolicyDenied(
                SATELLITE_TARGET_NOT_AUTHORIZED.into(),
            ));
        }
        store.queue_satellite_message(caller, &request, chrono::Utc::now())
    }
}
