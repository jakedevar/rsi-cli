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
        self.agent_get_provider_status_with_login(
            caller,
            request,
            service,
            vault,
            crate::provider_login::ProviderLoginService::global(),
        )
        .await
    }

    /// Same as [`Self::agent_get_provider_status_with`] with the login probes
    /// injected too (#2753).
    pub(crate) async fn agent_get_provider_status_with_login(
        &self,
        caller: Uuid,
        request: AgentGetProviderStatusRequestV1,
        service: &crate::provider_status::ProviderStatusService,
        vault: &crate::vault::VaultHandle,
        login: &crate::provider_login::ProviderLoginService,
    ) -> Result<AgentGetProviderStatusResultV1> {
        self.agent_get_provider_status_with_contract(
            caller,
            request,
            service,
            vault,
            login,
            crate::provider_contract::ProviderContractService::global(),
        )
        .await
    }

    /// Same as [`Self::agent_get_provider_status_with_login`] with the CLI
    /// contract canary injected too (#2755).
    pub(crate) async fn agent_get_provider_status_with_contract(
        &self,
        caller: Uuid,
        request: AgentGetProviderStatusRequestV1,
        service: &crate::provider_status::ProviderStatusService,
        vault: &crate::vault::VaultHandle,
        login: &crate::provider_login::ProviderLoginService,
        contract: &crate::provider_contract::ProviderContractService,
    ) -> Result<AgentGetProviderStatusResultV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let now = chrono::Utc::now();
        let (stats, unavailable, satellite_logins) = {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(PROVIDER_STATUS_NOT_AUTHORIZED.into()))?;
            if !(projection.is_lead || projection.is_manager) {
                return Err(DaemonError::PolicyDenied(
                    PROVIDER_STATUS_NOT_AUTHORIZED.into(),
                ));
            }
            let stats = store.provider_launch_stats(now)?;
            // #2735: a model the provider withdrew; advisory, so a failed read
            // leaves the report without it.
            let unavailable = store.models_unavailable(now).unwrap_or_default();
            // #2753: each paired satellite's own login probes; advisory too.
            let satellite_logins = store.satellite_provider_logins().unwrap_or_default();
            (stats, unavailable, satellite_logins)
        };
        // The store lock is released before any provider call.
        let mut report = service
            .report(vault, &stats, request.provider.as_deref(), now)
            .await;
        let live_unavailable = self.live_unavailable_codex_models(&unavailable).await;
        report.apply_models_unavailable(unavailable.into_iter().map(|record| {
            (
                record.provider,
                rsi_common::agent_provider_status::ProviderModelUnavailableV1 {
                    model: record.model,
                    code: rsi_common::agent_provider_status::PROVIDER_MODEL_UNAVAILABLE_CODE
                        .to_string(),
                    first_seen: record.first_seen,
                    last_seen: record.last_seen,
                    evidence_session_id: record.evidence_session_id,
                    evidence_stop_reason: record.evidence_stop_reason,
                },
            )
        }));
        // #2753: the CLI login state of this host, with each satellite's own
        // probe beside it. `unknown` is reported but never refuses.
        let logins = login.logins_for_status().await;
        report.apply_provider_login(logins.into_iter().map(|(name, mut state)| {
            state.hosts = rsi_common::provider_login::host_logins(&name, &satellite_logins);
            (name, state)
        }));
        // #2752: models this login's fresh live Codex list omits.
        report.apply_models_unavailable(live_unavailable);
        // #2755: the CLI contract canary state; an untrusted Codex custody hook
        // reads `launch_admission: refused` while the refusal setting is on.
        let refuse_untrusted_custody_hook = self
            .spawn_coordinator
            .runtime_config()
            .is_none_or(|config| config.codex_refuse_untrusted_custody_hook_enabled());
        report.apply_provider_contract(
            contract.contracts_for_status(),
            refuse_untrusted_custody_hook,
        );
        // #1407: report the operator provider profile and what it refuses.
        if let Some(config) = self.spawn_coordinator.runtime_config() {
            report.apply_provider_profile(config.provider_profile());
        }
        Ok(report)
    }

    /// #2752: the Codex models a fresh live catalog omits, one record per
    /// Codex-catalog provider, skipping models a stop-reason episode already
    /// reports. Advisory: no resolvable CLI or a failed probe adds nothing.
    pub(crate) async fn live_unavailable_codex_models(
        &self,
        reported: &[crate::store::provider_status::ModelUnavailable],
    ) -> Vec<(
        String,
        rsi_common::agent_provider_status::ProviderModelUnavailableV1,
    )> {
        let Some(config) = self.runtime_config.clone() else {
            return Vec::new();
        };
        let Ok(client) = crate::codex::CodexClient::new(config) else {
            return Vec::new();
        };
        let (view, models) = client.live_unavailable_models().await;
        let Some(fetched_at) = view.fetched_at else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for model in models {
            if reported
                .iter()
                .any(|record| record.provider == "Codex" && record.model == model)
            {
                continue;
            }
            for provider in ["Codex", "CodexAppServer"] {
                out.push((
                    provider.to_string(),
                    rsi_common::agent_provider_status::ProviderModelUnavailableV1 {
                        model: model.clone(),
                        code: rsi_common::agent_provider_status::PROVIDER_MODEL_UNAVAILABLE_CODE
                            .to_string(),
                        first_seen: fetched_at,
                        last_seen: fetched_at,
                        evidence_session_id: String::new(),
                        evidence_stop_reason: "live_catalog:absent".to_string(),
                    },
                ));
            }
        }
        out
    }
}
