//! API key resolution for the in-process Harness, through the key vault.
//!
//! Resolution order:
//! 1. Explicit key (from LaunchConfig)
//! 2. The model's vault slot via [`crate::vault::VaultHandle`] (vault entry,
//!    then — unless cleared — the slot's legacy env vars under
//!    `vault.env_compat`)
//! 3. Unless the slot is cleared or env compatibility is off, the generic
//!    legacy fallbacks (`RSI_API_KEY`, `MOTHERSHIP_API_KEY`,
//!    `FLYWHEEL_API_KEY`, `API_KEY`)

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use crate::vault::{SecretString, Slot, VaultHandle};

/// A Harness provider's credential, evaluated on every request so a vault
/// rotation or clear takes effect on the live session's next request
/// (#694 K1 rev4 F2). The key is never captured in `LaunchConfig` or the
/// provider struct as a plain string.
#[derive(Clone)]
pub enum ApiCredential {
    /// Keyless route (explicit local base URL without a key).
    None,
    /// Explicit key from `LaunchConfig` (legacy override; fixed for the run).
    Fixed(SecretString),
    /// The vault slot (then the generic legacy fallbacks), per request.
    Vault {
        vault: VaultHandle,
        slot: Option<Slot>,
    },
    VaultSlotOnly {
        vault: VaultHandle,
        slot: Slot,
    },
}

impl std::fmt::Debug for ApiCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("ApiCredential::None"),
            Self::Fixed(secret) => write!(formatter, "ApiCredential::Fixed({secret:?})"),
            Self::Vault { slot, .. } => write!(formatter, "ApiCredential::Vault({slot:?})"),
            Self::VaultSlotOnly { slot, .. } => {
                write!(formatter, "ApiCredential::VaultSlotOnly({slot:?})")
            }
        }
    }
}

impl ApiCredential {
    /// Explicit non-empty key wins (fixed); otherwise resolve `slot` through
    /// the daemon vault on every request.
    #[must_use]
    pub fn for_slot(explicit: Option<&str>, slot: Option<Slot>) -> Self {
        Self::for_slot_in(&crate::vault::global(), explicit, slot)
    }

    #[must_use]
    pub fn for_slot_in(vault: &VaultHandle, explicit: Option<&str>, slot: Option<Slot>) -> Self {
        explicit.filter(|key| !key.is_empty()).map_or_else(
            || Self::Vault {
                vault: vault.clone(),
                slot,
            },
            |key| Self::Fixed(SecretString::new(key.to_owned())),
        )
    }

    /// Explicit key only (no vault fallback), e.g. an explicit base URL.
    #[must_use]
    pub fn explicit(key: Option<&str>) -> Self {
        key.filter(|key| !key.is_empty()).map_or(Self::None, |key| {
            Self::Fixed(SecretString::new(key.to_owned()))
        })
    }

    #[must_use]
    pub fn for_slot_only(slot: Slot) -> Self {
        Self::VaultSlotOnly {
            vault: crate::vault::global(),
            slot,
        }
    }

    /// The credential for the request about to be sent.
    #[must_use]
    pub fn current(&self) -> Option<SecretString> {
        match self {
            Self::None => None,
            Self::Fixed(secret) => Some(secret.clone()),
            Self::Vault { vault, slot } => vault
                .resolve_with_generic_fallback(*slot)
                .map(|resolved| resolved.secret),
            Self::VaultSlotOnly { vault, slot } => vault
                .resolve(*slot)
                .ok()
                .flatten()
                .map(|resolved| resolved.secret),
        }
    }
}

/// Resolve an API key for a Harness provider slot (`None` = keyless route,
/// e.g. a local server).
///
/// Returns `None` if no key is found (valid for local models).
pub fn resolve_api_key(explicit: Option<&str>, slot: Option<Slot>) -> Option<String> {
    resolve_api_key_from(&crate::vault::global(), explicit, slot)
}

pub fn resolve_api_key_from(
    vault: &VaultHandle,
    explicit: Option<&str>,
    slot: Option<Slot>,
) -> Option<String> {
    // 1. Explicit key takes priority
    if let Some(key) = explicit.filter(|k| !k.is_empty()) {
        return Some(key.to_string());
    }
    // 2-3. Vault slot, then generic legacy fallbacks.
    vault
        .resolve_with_generic_fallback(slot)
        .map(|resolved| resolved.secret.expose().to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn vault(env: &'static [(&'static str, &'static str)]) -> VaultHandle {
        crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
            .env(move |name| {
                env.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            })
            .open()
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn test_explicit_key_takes_priority() {
        assert_eq!(
            resolve_api_key_from(
                &vault(&[("ANTHROPIC_API_KEY", "sk-test-env")]),
                Some("explicit-key"),
                Some(Slot::Anthropic)
            ),
            Some("explicit-key".to_string()),
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn test_empty_explicit_falls_through() {
        assert_eq!(
            resolve_api_key_from(&vault(&[]), Some(""), Some(Slot::Groq)),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn test_none_explicit_falls_through() {
        assert_eq!(
            resolve_api_key_from(&vault(&[]), None, Some(Slot::Groq)),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn slot_env_then_generic_fallback_preserved() {
        let env = vault(&[
            ("GROQ_API_KEY", "sk-test-groq"),
            ("RSI_API_KEY", "sk-test-generic"),
        ]);
        assert_eq!(
            resolve_api_key_from(&env, None, Some(Slot::Groq)),
            Some("sk-test-groq".to_string())
        );
        assert_eq!(
            resolve_api_key_from(&env, None, Some(Slot::Mistral)),
            Some("sk-test-generic".to_string())
        );
        // Keyless local route still honours the generic fallback.
        assert_eq!(
            resolve_api_key_from(&env, None, None),
            Some("sk-test-generic".to_string())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn vault_entry_wins_and_clear_suppresses_generic_fallback() {
        let env = vault(&[
            ("GROQ_API_KEY", "sk-test-groq"),
            ("RSI_API_KEY", "sk-test-generic"),
        ]);
        env.set(Slot::Groq, "sk-test-vault-groq").unwrap();
        assert_eq!(
            resolve_api_key_from(&env, None, Some(Slot::Groq)),
            Some("sk-test-vault-groq".to_string())
        );
        env.clear(Slot::Groq).unwrap();
        assert_eq!(resolve_api_key_from(&env, None, Some(Slot::Groq)), None);
    }

    /// #694 K1 rev4 F2: a vault rotation reaches the NEXT request of a live
    /// Harness provider, on both the non-streaming and the streaming path,
    /// for both the OpenAI-compatible and the Anthropic provider.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn rotation_reaches_next_request_of_live_harness_providers() {
        use crate::model_control::ModelExecutionCapability;
        use crate::model_control::registry::RuntimeExecutionRoute;
        use crate::session::harness::provider::ApiProvider;
        use crate::session::harness::providers::{
            anthropic::AnthropicProvider, openai_api::OpenAiApiProvider,
        };
        use crate::session::harness::types::{ChatMessage, ChatRequest, ProviderQuirks};

        let server = wiremock::MockServer::start().await;
        // Any status works: only the request headers are under test.
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(root.path().join("vault"))
                .env(|_| None)
                .open()
                .unwrap();
        vault.set(Slot::Openai, "sk-test-openai-old").unwrap();
        vault.set(Slot::Anthropic, "sk-test-anthropic-old").unwrap();
        let request = ChatRequest {
            messages: vec![ChatMessage::user("ping")],
            model: "test-model".into(),
            temperature: None,
            max_tokens: Some(8),
            tools: Vec::new(),
            stream: false,
            reasoning_effort: None,
        };
        let openai_route =
            || ModelExecutionCapability::for_test(RuntimeExecutionRoute::SessionHarnessOpenAiHttp);
        let anthropic_route = || {
            ModelExecutionCapability::for_test(RuntimeExecutionRoute::SessionHarnessAnthropicHttp)
        };

        let openai = OpenAiApiProvider::with_config(
            server.uri(),
            ApiCredential::for_slot_in(&vault, None, Some(Slot::Openai)),
            ProviderQuirks::default(),
        )
        .unwrap();
        let anthropic = AnthropicProvider::with_credential(
            server.uri(),
            ApiCredential::for_slot_in(&vault, None, Some(Slot::Anthropic)),
        )
        .unwrap();

        // Request 1 (non-streaming OpenAI, streaming Anthropic) with the old keys.
        let _ = openai.chat(&request, openai_route()).await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let _ = anthropic
            .stream_chat(
                &request,
                tx,
                &tokio_util::sync::CancellationToken::new(),
                anthropic_route(),
            )
            .await;

        vault.rotate(Slot::Openai, "sk-test-openai-new").unwrap();
        vault
            .rotate(Slot::Anthropic, "sk-test-anthropic-new")
            .unwrap();

        // Request 2 through the other path, same live provider instances.
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let _ = openai
            .stream_chat(
                &request,
                tx,
                &tokio_util::sync::CancellationToken::new(),
                openai_route(),
            )
            .await;
        let _ = anthropic.chat(&request, anthropic_route()).await;

        let received = server.received_requests().await.unwrap();
        let credentials: Vec<String> = received
            .iter()
            .map(|request| {
                let header = |name: &str| {
                    request
                        .headers
                        .get(name)
                        .map(|value| value.to_str().unwrap().to_owned())
                };
                header("authorization")
                    .or_else(|| header("x-api-key"))
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            credentials,
            vec![
                "Bearer sk-test-openai-old".to_string(),
                "sk-test-anthropic-old".to_string(),
                "Bearer sk-test-openai-new".to_string(),
                "sk-test-anthropic-new".to_string(),
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn explicit_key_is_fixed_and_vault_credential_follows_clear() {
        let env = vault(&[]);
        env.set(Slot::Openai, "sk-test-follow").unwrap();
        let fixed = ApiCredential::for_slot_in(&env, Some("sk-test-explicit"), Some(Slot::Openai));
        let live = ApiCredential::for_slot_in(&env, None, Some(Slot::Openai));
        assert_eq!(live.current().unwrap().expose(), "sk-test-follow");
        env.clear(Slot::Openai).unwrap();
        assert!(live.current().is_none());
        assert_eq!(fixed.current().unwrap().expose(), "sk-test-explicit");
        assert!(ApiCredential::explicit(Some("")).current().is_none());
        assert!(!format!("{fixed:?}").contains("sk-test-explicit"));
    }
}
