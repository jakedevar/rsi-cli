//! Saved custom-provider bindings survive same-ID resumes and successor chains.
//! Only the reference and nonsecret route are durable; keys are resolved anew.

use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::operator_settings::SessionCustomProviderBinding as Binding;
use crate::vault::VaultHandle;
use rsi_common::types::SessionProvider;
use uuid::Uuid;

/// Resolve before launch admission. Explicit inline overrides retain their
/// fresh-launch behavior. A launch-only key cannot be reconstructed later:
/// refuse that continuation rather than substituting an ambient credential.
pub(super) fn resolve(
    store: &Store,
    vault: &VaultHandle,
    config: &mut LaunchConfig,
    session_id: Option<Uuid>,
    explicit: Option<Uuid>,
) -> Result<Option<Binding>> {
    if let Some(id) = explicit {
        let (url, secret) =
            crate::operator_config::custom_providers::resolve_for_request(store, vault, id)?;
        let saved_key = secret.as_ref().map(|secret| secret.expose());
        if config.openai_base_url.as_deref().is_none_or(str::is_empty) {
            config.openai_base_url = Some(url.clone());
        }
        if config.openai_api_key.as_deref().is_none_or(str::is_empty) {
            config.openai_api_key = saved_key.map(str::to_owned);
        }
        config.provider = super::harness::provider::provider_for_explicit_endpoint(
            config.provider,
            config.openai_base_url.as_deref(),
        )?;
        let provider = config.provider.unwrap_or(SessionProvider::Claude);
        return Ok(Some(Binding {
            custom_provider_id: id,
            uses_saved_endpoint: config.openai_base_url.as_deref() == Some(url.as_str()),
            base_url: url,
            provider,
            uses_saved_credential: config.openai_api_key.as_deref() == saved_key,
            requires_credential: config.openai_api_key.is_some(),
        }));
    }
    let mut binding = session_id
        .map(|id| store.resolve_session_custom_provider_binding(id))
        .transpose()?
        .flatten();
    if binding.is_none() {
        binding = config
            .continued_from
            .map(|id| store.resolve_session_custom_provider_binding(id))
            .transpose()?
            .flatten();
    }
    let Some(binding) = binding else {
        return Ok(None);
    };
    if config
        .provider
        .is_some_and(|provider| provider != binding.provider)
    {
        return Err(DaemonError::InvalidParam(
            "custom_provider_runtime_changed: launch a new session with the desired provider"
                .into(),
        ));
    }
    config.provider = Some(binding.provider);
    if !binding.uses_saved_endpoint {
        return Err(DaemonError::InvalidParam("custom_provider_endpoint_unavailable: save the launch endpoint in Provider Setup and launch a new session".into()));
    }
    if !binding.uses_saved_credential {
        return Err(DaemonError::InvalidParam("custom_provider_credential_unavailable: save the launch credential in Provider Setup and launch a new session".into()));
    }
    let (saved_url, secret) = crate::operator_config::custom_providers::resolve_for_request(
        store,
        vault,
        binding.custom_provider_id,
    )?;
    if saved_url != binding.base_url {
        return Err(DaemonError::InvalidParam("custom_provider_endpoint_changed: launch a new session to use the edited provider endpoint".into()));
    }
    if binding.requires_credential && secret.is_none() {
        return Err(DaemonError::InvalidParam("custom_provider_credential_unavailable: restore the saved credential in Provider Setup".into()));
    }
    config.openai_base_url = Some(binding.base_url.clone());
    config.openai_api_key = secret.map(|secret| secret.expose().to_owned());
    Ok(None) // descendants resolve the ancestor; no copied binding or secret
}

/// Write ahead of session-row admission. This settings namespace has no
/// session foreign key: retaining an unused nonsecret binding is safe, while
/// publishing a resumable session before its binding can silently lose it.
pub(super) fn persist(
    store: &Store,
    session_id: Uuid,
    mut binding: Binding,
    provider: SessionProvider,
) -> Result<()> {
    binding.provider = provider;
    store.set_session_custom_provider_binding(session_id, &binding)
}

pub(super) async fn restore(
    store: &std::sync::Arc<rsid_core::store_lock::Mutex<Store>>,
    config: &mut LaunchConfig,
) -> Result<()> {
    let store = store.lock().await;
    resolve(
        &store,
        &crate::vault::global(),
        config,
        config.rsi_session_id,
        None,
    )
    .map(drop)
}
