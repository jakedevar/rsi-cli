//! Closed slot table: legacy env var names, routes and consumer mappings.

use rsi_common::provider_credentials::{CliExposure, CredentialRoute};
use rsi_common::types::SessionProvider;

pub use rsi_common::provider_credentials::ProviderCredentialSlot as Slot;

/// Generic legacy fallbacks the in-process Harness has always honoured.
///
/// They come after the provider-specific vars and belong to no slot, so they
/// are never imported, but they are always scrubbed from child processes.
pub const GENERIC_FALLBACK_ENV_VARS: &[&str] = &[
    "RSI_API_KEY",
    "MOTHERSHIP_API_KEY",
    "FLYWHEEL_API_KEY",
    "API_KEY",
];

/// API keys the daemon itself reads for its own services (not vault slots).
///
/// They are never injected into a child, only scrubbed. Deliberately excluded:
/// `RSI_SESSION_TOKEN` and the other rsi identity/transport vars, which the
/// stamp chokepoint sets on purpose.
pub const DAEMON_SERVICE_KEY_ENV_VARS: &[&str] = &[
    // Local provider / memory LLM (`openai.rs`, `memory/llm.rs`).
    "LOCAL_LLM_API_KEY",
    // `config.rs` `env_var_legacy!` keys: RSI_ plus legacy aliases.
    "RSI_MEMORY_EMBEDDING_API_KEY",
    "MOTHERSHIP_MEMORY_EMBEDDING_API_KEY",
    "FLYWHEEL_MEMORY_EMBEDDING_API_KEY",
    "RSI_DREAM_API_KEY",
    "MOTHERSHIP_DREAM_API_KEY",
    "FLYWHEEL_DREAM_API_KEY",
    "RSI_DIALECTIC_KEY",
    "MOTHERSHIP_DIALECTIC_KEY",
    "FLYWHEEL_DIALECTIC_KEY",
    "RSI_STALL_CLASSIFIER_API_KEY",
    "MOTHERSHIP_STALL_CLASSIFIER_API_KEY",
    "FLYWHEEL_STALL_CLASSIFIER_API_KEY",
    "RSI_LINEAR_API_KEY",
    "MOTHERSHIP_LINEAR_API_KEY",
    "FLYWHEEL_LINEAR_API_KEY",
];

/// Legacy env var names for a slot, in resolution-precedence order. The first
/// name is the one injected into a child process that needs the credential.
#[must_use]
pub const fn env_vars(slot: Slot) -> &'static [&'static str] {
    match slot {
        // `OPEN_ROUTER` is the Codex-route var; the Harness compatible table
        // historically read `OPENROUTER_API_KEY`.
        Slot::Openrouter => &["OPEN_ROUTER", "OPENROUTER_API_KEY"],
        Slot::Anthropic => &["ANTHROPIC_API_KEY"],
        Slot::Openai => &["OPENAI_API_KEY"],
        Slot::Bedrock => &["AWS_BEARER_TOKEN_BEDROCK"],
        Slot::Pioneer => &["PIONEER_AI_INFERENCE", "PIONEER_API_KEY"],
        Slot::Inception => &["INCEPTION_API_KEY", "MERCURY_API_KEY"],
        Slot::Deepseek => &["DEEPSEEK_API_KEY"],
        Slot::Groq => &["GROQ_API_KEY"],
        Slot::Mistral => &["MISTRAL_API_KEY"],
        Slot::Xai => &["XAI_API_KEY", "GROK_API_KEY"],
        Slot::Cerebras => &["CEREBRAS_API_KEY"],
        Slot::Perplexity => &["PERPLEXITY_API_KEY"],
        Slot::Together => &["TOGETHER_API_KEY"],
        Slot::Fireworks => &["FIREWORKS_API_KEY"],
        Slot::Cohere => &["COHERE_API_KEY"],
        Slot::Sambanova => &["SAMBANOVA_API_KEY"],
        Slot::Lepton => &["LEPTON_API_KEY"],
        Slot::Nvidia => &["NVIDIA_API_KEY"],
        Slot::Siliconflow => &["SILICONFLOW_API_KEY"],
        Slot::Huggingface => &["HF_API_KEY", "HUGGINGFACE_API_KEY"],
        Slot::Cloudflare => &["CLOUDFLARE_API_KEY"],
        Slot::Anyscale => &["ANYSCALE_API_KEY"],
        Slot::Octoai => &["OCTOAI_API_KEY"],
    }
}

/// Every credential env var name that must never reach an agent-facing
/// process unless a launch deliberately injects it.
pub fn scrubbed_env_var_names() -> impl Iterator<Item = &'static str> {
    Slot::ALL
        .into_iter()
        .flat_map(|slot| env_vars(slot).iter().copied())
        .chain(GENERIC_FALLBACK_ENV_VARS.iter().copied())
        .chain(DAEMON_SERVICE_KEY_ENV_VARS.iter().copied())
}

/// The single routing helper slice R extends. Until R lands, the providers
/// that own a Codex-launched slot always use the Codex CLI; every other slot
/// is consumed only by the in-process Harness.
#[must_use]
pub const fn route(slot: Slot) -> CredentialRoute {
    match slot {
        Slot::Openrouter | Slot::Bedrock | Slot::Pioneer => CredentialRoute::CodexCli,
        _ => CredentialRoute::Harness,
    }
}

/// Slots some daemon CLI consumer can receive: the session Codex route and
/// the memory LLM's Codex CLI both serve `openrouter`, `bedrock` and
/// `pioneer`.
#[must_use]
pub const fn has_cli_consumer(slot: Slot) -> bool {
    matches!(slot, Slot::Openrouter | Slot::Bedrock | Slot::Pioneer)
}

/// Slots with a Harness-to-Codex launch fallback (slice R's scope).
#[must_use]
pub const fn has_codex_fallback(slot: Slot) -> bool {
    matches!(slot, Slot::Openrouter)
}

/// `api_route.fallback` default. Slice R wires the live setting.
pub const CODEX_FALLBACK_DEFAULT: bool = true;

/// Capability-derived residual exposure R1 for `slot`.
#[must_use]
pub const fn cli_exposure(slot: Slot) -> CliExposure {
    derive_cli_exposure(
        route(slot),
        has_cli_consumer(slot),
        has_codex_fallback(slot),
        CODEX_FALLBACK_DEFAULT,
    )
}

/// `always` when the route is `codex_cli` or any CLI consumer uses the slot;
/// `on_fallback` for a harness route whose Codex fallback is enabled; `none`
/// only when no CLI can receive the key.
#[must_use]
pub const fn derive_cli_exposure(
    route: CredentialRoute,
    cli_consumer: bool,
    fallback_path: bool,
    fallback_enabled: bool,
) -> CliExposure {
    match route {
        CredentialRoute::CodexCli => CliExposure::Always,
        CredentialRoute::Harness if cli_consumer => CliExposure::Always,
        CredentialRoute::Harness if fallback_path && fallback_enabled => CliExposure::OnFallback,
        CredentialRoute::Harness => CliExposure::None,
    }
}

/// The slot a Codex-launched provider authenticates with, if any.
#[must_use]
pub const fn slot_for_provider(provider: SessionProvider) -> Option<Slot> {
    match provider {
        SessionProvider::OpenRouter => Some(Slot::Openrouter),
        SessionProvider::Bedrock => Some(Slot::Bedrock),
        SessionProvider::Pioneer => Some(Slot::Pioneer),
        _ => None,
    }
}

/// The slot the in-process Harness uses for `model` (no explicit base URL).
#[must_use]
pub fn slot_for_harness_model(model: &str) -> Option<Slot> {
    if crate::bedrock::bedrock_vendor(model).is_some() {
        return Some(Slot::Bedrock);
    }
    if model.starts_with("claude-") {
        return Some(Slot::Anthropic);
    }
    if ["gpt-", "o1-", "o3-", "o4-"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return Some(Slot::Openai);
    }
    crate::store_support::compatible_table::lookup(model)
        .filter(|entry| !entry.env_vars.is_empty())
        .and_then(|entry| slot_for_compatible_entry(entry.name))
}

/// Maps a Harness compatible-table entry name to its slot. Keyless local
/// entries (lmstudio, llamacpp, vllm, ollama) have none.
#[must_use]
pub fn slot_for_compatible_entry(name: &str) -> Option<Slot> {
    match name {
        "mercury" => Some(Slot::Inception),
        other => Slot::parse(other),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn every_keyed_compatible_entry_maps_to_a_slot_covering_its_env_vars() {
        for entry in crate::store_support::compatible_table::COMPATIBLE_TABLE {
            match slot_for_compatible_entry(entry.name) {
                Some(slot) => {
                    for var in entry.env_vars {
                        assert!(
                            env_vars(slot).contains(var),
                            "{} var {var} missing from slot {slot}",
                            entry.name
                        );
                    }
                }
                None => assert!(
                    entry.env_vars.is_empty(),
                    "keyed entry {} has no slot",
                    entry.name
                ),
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn scrub_names_cover_every_slot_and_generic_fallback() {
        let names: Vec<_> = scrubbed_env_var_names().collect();
        for required in [
            "OPEN_ROUTER",
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_BEARER_TOKEN_BEDROCK",
            "PIONEER_AI_INFERENCE",
            "PIONEER_API_KEY",
            "RSI_API_KEY",
            "API_KEY",
            "MOTHERSHIP_API_KEY",
            "FLYWHEEL_API_KEY",
            "LOCAL_LLM_API_KEY",
            "RSI_DREAM_API_KEY",
            "MOTHERSHIP_DREAM_API_KEY",
            "FLYWHEEL_DREAM_API_KEY",
            "RSI_STALL_CLASSIFIER_API_KEY",
            "MOTHERSHIP_STALL_CLASSIFIER_API_KEY",
            "FLYWHEEL_STALL_CLASSIFIER_API_KEY",
            "RSI_MEMORY_EMBEDDING_API_KEY",
            "RSI_DIALECTIC_KEY",
            "RSI_LINEAR_API_KEY",
        ] {
            assert!(names.contains(&required), "{required}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn route_is_codex_cli_only_for_codex_launched_slots() {
        for slot in Slot::ALL {
            let expected = matches!(slot, Slot::Openrouter | Slot::Bedrock | Slot::Pioneer);
            assert_eq!(route(slot) == CredentialRoute::CodexCli, expected, "{slot}");
            let exposure = if expected {
                CliExposure::Always
            } else {
                CliExposure::None
            };
            assert_eq!(cli_exposure(slot), exposure, "{slot}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn cli_exposure_is_derived_from_capability() {
        use CredentialRoute::{CodexCli, Harness};
        assert_eq!(
            derive_cli_exposure(CodexCli, false, false, false),
            CliExposure::Always
        );
        // A memory-LLM CLI consumer keeps a harness-routed slot `always`.
        assert_eq!(
            derive_cli_exposure(Harness, true, true, true),
            CliExposure::Always
        );
        assert_eq!(
            derive_cli_exposure(Harness, false, true, true),
            CliExposure::OnFallback
        );
        assert_eq!(
            derive_cli_exposure(Harness, false, true, false),
            CliExposure::None
        );
        assert_eq!(
            derive_cli_exposure(Harness, false, false, true),
            CliExposure::None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn harness_models_map_to_slots() {
        assert_eq!(
            slot_for_harness_model("claude-sonnet-5"),
            Some(Slot::Anthropic)
        );
        assert_eq!(slot_for_harness_model("gpt-5.2"), Some(Slot::Openai));
        assert_eq!(slot_for_harness_model("mercury-2"), Some(Slot::Inception));
        assert_eq!(slot_for_harness_model("mystery-model"), None);
    }
}
