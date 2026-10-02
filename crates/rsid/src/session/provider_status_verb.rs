//! `AgentGetProviderStatus` (#1044): secret-free provider health for the
//! current appointed manager or the current lead of the caller's Epic.
//!
//! Authority is the same projection `AgentEnqueueLandingSource` uses; workers
//! are refused `provider_status_not_authorized`. The daemon makes the provider
//! calls with its own credentials and never returns them.

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use rsi_common::agent_provider_status::{
    AgentGetProviderStatusRequestV1, AgentGetProviderStatusResultV1,
};
use uuid::Uuid;

/// Stable refusal for a caller that is neither manager nor Epic lead.
pub const PROVIDER_STATUS_NOT_AUTHORIZED: &str = "provider_status_not_authorized";

impl AgentControlHandle {
    /// # Errors
    /// A stable `provider_status_*` refusal, or a persistence error.
    pub async fn agent_get_provider_status(
        &self,
        caller: Uuid,
        request: AgentGetProviderStatusRequestV1,
    ) -> Result<AgentGetProviderStatusResultV1> {
        self.agent_get_provider_status_with(
            caller,
            request,
            crate::provider_status::ProviderStatusService::global(),
            &crate::vault::global(),
        )
        .await
    }

    /// Same as [`Self::agent_get_provider_status`] with the service and vault
    /// injected, so a test can stub the provider API.
    pub(crate) async fn agent_get_provider_status_with(
        &self,
        caller: Uuid,
        request: AgentGetProviderStatusRequestV1,
        service: &crate::provider_status::ProviderStatusService,
        vault: &crate::vault::VaultHandle,
    ) -> Result<AgentGetProviderStatusResultV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let now = chrono::Utc::now();
        let stats = {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(PROVIDER_STATUS_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager) {
                return Err(DaemonError::PolicyDenied(
                    PROVIDER_STATUS_NOT_AUTHORIZED.into(),
                ));
            }
            store.provider_launch_stats(now)?
        };
        // The store lock is released before any provider call.
        Ok(service
            .report(vault, &stats, request.provider.as_deref(), now)
            .await)
    }
}
