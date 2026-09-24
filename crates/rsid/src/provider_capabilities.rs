//! Canonical daemon-side provider capability registry and resolver.
//!
//! The shared DTOs in `rsi-common` define the wire vocabulary. This module
//! owns daemon evidence: repository fallbacks, the parsed installed Codex
//! catalog cache, the reviewed capacity-contract allowlist, and the one
//! precedence path that resolves an active budget.

use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use rsi_common::types::{Session, SessionProvider};
use rsi_common::{
    CapabilityConfidence, CapabilityEvidence, CapabilitySource, ContextCapacity,
    ResolvedContextBudget, Sha256Digest,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, LazyLock};

const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 128_000;
const CODEX_EFFECTIVE_FALLBACK_TOKENS: u64 = 258_400;
const REPOSITORY_FALLBACK_VERSION: &str = "rsi-provider-capabilities-v1";
pub(crate) const MAX_VALIDATED_CONTEXT_TOKENS: u64 = 10_000_000;
pub(crate) const VALIDATED_CODEX_CLI_VERSION: &str = "codex-cli 0.155.1";
pub(crate) const VALIDATED_CODEX_RAW_CATALOG_DIGEST: &str =
    "sha256:404c1dd19656240244e0ae74d7e82326bd192ec4c2a0e6f32d130075fa1fb0f4";
pub(crate) const VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST: &str =
    "sha256:a60498398ac4824e943151d11e2479b003eb971b4180fdcd862d49fcdc577321";
/// Exact catalog contracts whose schema and semantics have passed offline
/// review. The raw installed catalog and its bounded semantic fixture are two
/// reviewed representations of the same 0.155.1 contract. A new CLI version
/// or digest remains discovery-only until this allowlist is deliberately
/// extended alongside provider-capability validation.
pub(crate) const VALIDATED_CODEX_CATALOG_CONTRACTS: &[(&str, &str)] = &[
    (
        VALIDATED_CODEX_CLI_VERSION,
        VALIDATED_CODEX_RAW_CATALOG_DIGEST,
    ),
    (
        VALIDATED_CODEX_CLI_VERSION,
        VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogFieldHandling {
    Consumed(&'static str),
    Ignored(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatalogFieldContract {
    pub(crate) key: &'static str,
    pub(crate) category: &'static str,
    pub(crate) handling: CatalogFieldHandling,
}

macro_rules! consumed {
    ($key:literal, $category:literal, $projection:literal) => {
        CatalogFieldContract {
            key: $key,
            category: $category,
            handling: CatalogFieldHandling::Consumed($projection),
        }
    };
}

macro_rules! ignored {
    ($key:literal, $category:literal, $reason:literal) => {
        CatalogFieldContract {
            key: $key,
            category: $category,
            handling: CatalogFieldHandling::Ignored($reason),
        }
    };
}

/// Closed schema inventory for the installed Codex 0.155.1 catalog.
pub(crate) const CODEX_CATALOG_MODEL_FIELDS: &[CatalogFieldContract] = &[
    ignored!(
        "additional_speed_tiers",
        "service",
        "service-tier presentation is outside context capability resolution"
    ),
    consumed!(
        "apply_patch_tool_type",
        "tools",
        "CodexCatalogToolCapabilities.apply_patch_tool_type"
    ),
    ignored!(
        "availability_nux",
        "presentation",
        "provider onboarding state is not a daemon capability"
    ),
    ignored!(
        "base_instructions",
        "prompt",
        "large provider prompt payload is private to the installed CLI"
    ),
    consumed!(
        "comp_hash",
        "compaction",
        "CodexCatalogCompactionMetadata.comp_hash"
    ),
    consumed!(
        "context_window",
        "context",
        "ContextCapacity.provider_default_tokens"
    ),
    consumed!(
        "default_reasoning_level",
        "effort",
        "CodexCatalogModel.default_reasoning_level"
    ),
    ignored!(
        "default_reasoning_summary",
        "presentation",
        "reasoning-summary presentation does not change capacity"
    ),
    ignored!(
        "default_verbosity",
        "presentation",
        "response verbosity does not change context capacity"
    ),
    consumed!("description", "identity", "CodexCatalogModel.description"),
    consumed!("display_name", "identity", "CodexCatalogModel.display_name"),
    consumed!(
        "effective_context_window_percent",
        "context",
        "ContextCapacity.effective_percent"
    ),
    consumed!(
        "experimental_supported_tools",
        "tools",
        "CodexCatalogToolCapabilities.experimental_supported_tools"
    ),
    ignored!(
        "include_apps_usage_instructions",
        "prompt",
        "installed CLI owns app-instruction assembly"
    ),
    ignored!(
        "include_plugin_usage_instructions",
        "prompt",
        "installed CLI owns plugin-instruction assembly"
    ),
    ignored!(
        "include_skills_usage_instructions",
        "prompt",
        "installed CLI owns skill-instruction assembly"
    ),
    consumed!(
        "input_modalities",
        "modalities",
        "CodexCatalogToolCapabilities.input_modalities"
    ),
    consumed!(
        "max_context_window",
        "context",
        "ContextCapacity.provider_max_tokens"
    ),
    ignored!(
        "model_messages",
        "prompt",
        "provider-owned message payload is intentionally absent from fixtures"
    ),
    ignored!(
        "multi_agent_reasoning_effort",
        "routing",
        "installed CLI owns its delegated-worker reasoning policy"
    ),
    ignored!(
        "multi_agent_version",
        "routing",
        "installed CLI owns its multi-agent protocol selection"
    ),
    ignored!(
        "node_repl_auto_review_required",
        "tools",
        "installed CLI owns Node REPL review policy"
    ),
    ignored!(
        "node_repl_disabled",
        "tools",
        "installed CLI owns Node REPL enablement"
    ),
    consumed!("priority", "visibility", "CodexCatalogModel.priority"),
    ignored!(
        "service_tiers",
        "service",
        "billing and latency tiers are not context-capacity evidence"
    ),
    consumed!(
        "shell_type",
        "tools",
        "CodexCatalogToolCapabilities.shell_type"
    ),
    consumed!("slug", "identity", "CodexCatalogModel.slug"),
    ignored!(
        "support_verbosity",
        "presentation",
        "verbosity support is not used by daemon context resolution"
    ),
    consumed!(
        "supported_in_api",
        "visibility",
        "CodexCatalogModel.supported_in_api"
    ),
    consumed!(
        "supported_reasoning_levels",
        "effort",
        "CodexCatalogModel.supported_reasoning_levels"
    ),
    ignored!(
        "supports_experimental_context",
        "context",
        "installed CLI owns experimental-context behavior"
    ),
    consumed!(
        "supports_image_detail_original",
        "modalities",
        "CodexCatalogToolCapabilities.supports_image_detail_original"
    ),
    consumed!(
        "supports_search_tool",
        "tools",
        "CodexCatalogToolCapabilities.supports_search_tool"
    ),
    consumed!(
        "tool_mode",
        "tools",
        "CodexCatalogToolCapabilities.tool_mode"
    ),
    consumed!(
        "truncation_policy",
        "compaction",
        "CodexCatalogCompactionMetadata.truncation_policy"
    ),
    ignored!(
        "upgrade",
        "presentation",
        "provider upgrade messaging is not capability evidence"
    ),
    ignored!(
        "use_responses_lite",
        "transport",
        "installed CLI owns its Responses transport selection"
    ),
    consumed!("visibility", "visibility", "CodexCatalogModel.visibility"),
    consumed!(
        "web_search_tool_type",
        "tools",
        "CodexCatalogToolCapabilities.web_search_tool_type"
    ),
];

pub(crate) const CODEX_CATALOG_ROOT_FIELDS: &[CatalogFieldContract] = &[consumed!(
    "models",
    "catalog",
    "CodexCatalogSnapshot.models"
)];

/// Versioned descriptive product facts. These enrich resolved capacity detail
/// but never select an active provider-session denominator by themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfficialModelCapacity {
    pub(crate) slug: &'static str,
    pub(crate) advertised_max_tokens: u64,
    pub(crate) max_output_tokens: u64,
    pub(crate) documentation_url: &'static str,
}

pub(crate) const OFFICIAL_MODEL_CAPACITIES: &[OfficialModelCapacity] = &[OfficialModelCapacity {
    slug: "gpt-6-astra",
    advertised_max_tokens: 1_050_000,
    max_output_tokens: 128_000,
    documentation_url: "https://developers.openai.com/api/docs/models/gpt-6-astra",
}];

/// Claude ids the picker no longer offers but that live sessions still carry.
///
/// Retiring a catalog entry must not silently re-size sessions already running
/// on it: `claude-fable-5` was replaced in the catalog by `claude-fable-5-1`,
/// and without this map it falls through to the 128k default — the F-130
/// failure mode (context fill overstated ~8x, driving premature rotation).
///
/// Deliberately Claude-scoped and exact-match rather than a row in
/// [`REPOSITORY_MODEL_FALLBACKS`], which is provider-agnostic and matched by
/// substring: a `fable` row there would also answer a Claude id launched under
/// `Local`, which must keep resolving to the unverified default.
pub(crate) const RETIRED_CLAUDE_MODEL_WINDOWS: &[(&str, u64)] = &[("claude-fable-5", 1_000_000)];

/// The only static model-pattern registry in the daemon.
///
/// Matching is case-insensitive. More-specific entries must precede family
/// fallbacks. Provider-specific Codex transport overrides live beside this
/// table below rather than in session or monitor consumers.
pub(crate) const REPOSITORY_MODEL_FALLBACKS: &[(&str, u64)] = &[
    ("opus-4-7-200k", 200_000),
    ("opus-4.7-200k", 200_000),
    ("opus-4.7", 1_000_000),
    ("opus-4-7", 1_000_000),
    ("opus-4-6-200k", 200_000),
    ("opus-4.6-200k", 200_000),
    ("opus-4.6", 1_000_000),
    ("opus-4-6", 1_000_000),
    ("opus-4", 1_000_000),
    ("sonnet-5", 1_000_000),
    ("haiku-4", 200_000),
    ("opus-3", 200_000),
    ("haiku-3.5", 200_000),
    ("qwen3.6", 262_144),
    ("qwen3-coder-next", 262_144),
    ("qwen3", 32_768),
    ("phi4", 16_384),
    ("phi-4", 16_384),
    ("deepseek", 64_000),
    ("gpt-5.4-mini", 400_000),
    ("gpt-6", 1_050_000),
    ("gpt-5.5", 1_050_000),
    ("gpt-5.4", 1_050_000),
    ("gpt-5.3-codex", 400_000),
    ("gpt-5.2-codex", 400_000),
    ("gpt-5.2", 272_000),
    ("gpt-5.1-codex", 400_000),
    ("gpt-5-codex", 400_000),
    ("gpt-5-mini", 400_000),
    ("gpt-5", 400_000),
    ("gpt-4.1", 1_047_576),
    ("o4-mini", 200_000),
    ("qwen2.5", 131_072),
    ("gemma4", 262_144),
    ("gemma-4", 262_144),
    ("gemma-3", 128_000),
    ("gemma3", 128_000),
    ("llama-3", 131_072),
    ("glm", 128_000),
    ("minimax", 100_000),
    ("gemini-2.5", 1_048_576),
    ("gemini-3", 1_048_576),
    ("gemini-3.5", 1_048_576),
];

pub(crate) const CODEX_TRANSPORT_FALLBACKS: &[(&str, u64)] = &[
    ("gpt-6", CODEX_EFFECTIVE_FALLBACK_TOKENS),
    ("gpt-5.5", 272_000),
    ("gpt-5.4", 272_000),
    ("gpt-5.2", 272_000),
    ("gpt-5", CODEX_EFFECTIVE_FALLBACK_TOKENS),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexReasoningLevel {
    pub(crate) effort: String,
    pub(crate) description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexCatalogToolCapabilities {
    pub(crate) input_modalities: Vec<String>,
    pub(crate) apply_patch_tool_type: Option<String>,
    pub(crate) experimental_supported_tools: Vec<Value>,
    pub(crate) shell_type: Option<String>,
    pub(crate) supports_image_detail_original: Option<bool>,
    pub(crate) supports_search_tool: Option<bool>,
    pub(crate) tool_mode: Option<String>,
    pub(crate) web_search_tool_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexCatalogCompactionMetadata {
    /// Provider configuration identity, not a token threshold.
    pub(crate) comp_hash: Option<String>,
    /// Tool/output truncation metadata, not the session compaction limit.
    pub(crate) truncation_policy: Option<Value>,
}

/// Validated semantic projection of one raw Codex catalog model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexCatalogModel {
    pub(crate) slug: String,
    pub(crate) display_name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) visibility: Option<String>,
    pub(crate) priority: Option<i64>,
    pub(crate) supported_in_api: Option<bool>,
    pub(crate) capacity: ContextCapacity,
    pub(crate) default_reasoning_level: Option<String>,
    pub(crate) supported_reasoning_levels: Vec<CodexReasoningLevel>,
    pub(crate) tools: CodexCatalogToolCapabilities,
    pub(crate) compaction: CodexCatalogCompactionMetadata,
    /// Exact object keys retained for later provider-catalog drift validation.
    pub(crate) raw_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexCatalogCacheKey {
    pub(crate) cli_version: String,
    pub(crate) content_digest: Sha256Digest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexCatalogTrust {
    /// Exact version, schema, and semantic contract passed offline review.
    Allowlisted,
    /// Structurally parseable and usable for discovery, but not capacity.
    DiscoveryOnly,
}

/// One structurally parsed installed Codex catalog observation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexCatalogSnapshot {
    pub(crate) key: CodexCatalogCacheKey,
    pub(crate) trust: CodexCatalogTrust,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) models: Vec<CodexCatalogModel>,
    pub(crate) raw_keys: BTreeSet<String>,
}

impl CodexCatalogSnapshot {
    pub(crate) fn legacy_model_tuples(&self) -> Vec<(String, String)> {
        let mut entries = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, model)| model.visibility.as_deref() == Some("list"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|(index, model)| (model.priority.unwrap_or(i64::MAX), *index));

        let mut seen = HashSet::new();
        entries
            .into_iter()
            .filter(|(_, model)| seen.insert(model.slug.clone()))
            .map(|(_, model)| {
                let label = model
                    .display_name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| rsi_common::model_utils::abbreviate_model(&model.slug));
                (model.slug.clone(), label)
            })
            .collect()
    }

    fn model(&self, slug: &str) -> Option<&CodexCatalogModel> {
        let slug = canonical_capability_model_slug(slug);
        self.models
            .iter()
            .find(|model| model.slug.eq_ignore_ascii_case(slug))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogRefreshReason {
    Startup,
    ExplicitDiscovery,
    VersionChange,
}

#[derive(Debug, Clone)]
pub(crate) enum CodexCatalogRefresh {
    Updated(Arc<CodexCatalogSnapshot>),
    Reused(Arc<CodexCatalogSnapshot>),
}

impl CodexCatalogRefresh {
    pub(crate) fn snapshot(&self) -> &Arc<CodexCatalogSnapshot> {
        match self {
            Self::Updated(snapshot) | Self::Reused(snapshot) => snapshot,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CatalogProbeState {
    Never,
    Parsed(CodexCatalogCacheKey),
    Invalid {
        cli_version: Option<String>,
        content_digest: Option<Sha256Digest>,
    },
}

impl Default for CatalogProbeState {
    fn default() -> Self {
        Self::Never
    }
}

#[derive(Debug, Default)]
struct CodexCatalogCacheState {
    parsed: Option<Arc<CodexCatalogSnapshot>>,
    last_probe: CatalogProbeState,
}

/// Process-local structurally parsed catalog cache.
///
/// Every parseable snapshot remains available to model discovery. Only an
/// exact allowlisted contract may supply resolved capacity. Failed probes
/// update freshness state but never replace the last parsed snapshot, so model
/// menus may reuse the last good projection while capacity resolution degrades.
#[derive(Debug, Default)]
pub(crate) struct CodexCatalogCache {
    state: RwLock<CodexCatalogCacheState>,
}

impl CodexCatalogCache {
    pub(crate) fn refresh_from_bytes(
        &self,
        cli_version: &str,
        raw: &[u8],
        observed_at: DateTime<Utc>,
    ) -> Result<CodexCatalogRefresh> {
        let cli_version = cli_version.trim();
        let content_digest = content_digest(raw);
        if cli_version.is_empty() {
            self.record_probe_failure(None, Some(content_digest));
            return Err(DaemonError::Process(
                "Codex CLI version probe returned an empty version".to_string(),
            ));
        }
        let key = CodexCatalogCacheKey {
            cli_version: cli_version.to_string(),
            content_digest,
        };
        let snapshot = match parse_codex_catalog_snapshot(cli_version, raw, observed_at) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.record_probe_failure(
                    Some(cli_version.to_string()),
                    Some(key.content_digest.clone()),
                );
                return Err(error);
            }
        };

        let mut state = self.state.write();
        if let Some(existing) = state
            .parsed
            .as_ref()
            .filter(|existing| existing.key == key)
            .cloned()
        {
            state.last_probe = CatalogProbeState::Parsed(key);
            return Ok(CodexCatalogRefresh::Reused(existing));
        }

        let snapshot = Arc::new(snapshot);
        state.parsed = Some(Arc::clone(&snapshot));
        state.last_probe = CatalogProbeState::Parsed(key);
        Ok(CodexCatalogRefresh::Updated(snapshot))
    }

    pub(crate) fn record_probe_failure(
        &self,
        cli_version: Option<String>,
        content_digest: Option<Sha256Digest>,
    ) {
        self.state.write().last_probe = CatalogProbeState::Invalid {
            cli_version,
            content_digest,
        };
    }

    pub(crate) fn cached_snapshot(&self) -> Option<Arc<CodexCatalogSnapshot>> {
        self.state.read().parsed.clone()
    }

    pub(crate) fn current_snapshot_for_version(
        &self,
        expected_cli_version: Option<&str>,
    ) -> Option<Arc<CodexCatalogSnapshot>> {
        self.current_snapshot_for_observation(expected_cli_version, None)
    }

    fn current_snapshot_for_observation(
        &self,
        expected_cli_version: Option<&str>,
        expected_content_digest: Option<&str>,
    ) -> Option<Arc<CodexCatalogSnapshot>> {
        let state = self.state.read();
        let snapshot = state.parsed.as_ref()?;
        let CatalogProbeState::Parsed(observed_key) = &state.last_probe else {
            return None;
        };
        if observed_key != &snapshot.key
            || expected_cli_version
                .is_some_and(|version| version.trim() != snapshot.key.cli_version)
            || expected_content_digest
                .is_some_and(|digest| digest.trim() != snapshot.key.content_digest.as_str())
        {
            return None;
        }
        Some(Arc::clone(snapshot))
    }

    fn current_allowlisted_snapshot_for_version(
        &self,
        expected_cli_version: Option<&str>,
    ) -> Option<Arc<CodexCatalogSnapshot>> {
        self.current_snapshot_for_version(expected_cli_version)
            .filter(|snapshot| snapshot.trust == CodexCatalogTrust::Allowlisted)
    }

    pub(crate) fn current_snapshot_for_exact_version(
        &self,
        cli_version: &str,
    ) -> Option<Arc<CodexCatalogSnapshot>> {
        self.current_snapshot_for_version(Some(cli_version))
    }

    fn current_allowlisted_snapshot_for_exact_version(
        &self,
        cli_version: &str,
    ) -> Option<Arc<CodexCatalogSnapshot>> {
        self.current_allowlisted_snapshot_for_version(Some(cli_version))
    }

    fn has_unusable_capability_observation(
        &self,
        expected_cli_version: Option<&str>,
        expected_content_digest: Option<&str>,
    ) -> bool {
        let state = self.state.read();
        match (&state.parsed, &state.last_probe) {
            (_, CatalogProbeState::Invalid { .. }) => true,
            (Some(snapshot), CatalogProbeState::Parsed(observed_key)) => {
                observed_key != &snapshot.key
                    || expected_cli_version
                        .is_some_and(|version| version.trim() != snapshot.key.cli_version)
                    || expected_content_digest
                        .is_some_and(|digest| digest.trim() != snapshot.key.content_digest.as_str())
                    || snapshot.trust != CodexCatalogTrust::Allowlisted
            }
            (Some(_), CatalogProbeState::Never) => true,
            (None, CatalogProbeState::Parsed(_)) => true,
            (None, CatalogProbeState::Never) => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ContextBudgetRequest<'a> {
    pub(crate) provider: SessionProvider,
    pub(crate) model: &'a str,
    pub(crate) configured_tokens: Option<u64>,
    pub(crate) runtime_effective_tokens: Option<u64>,
    pub(crate) legacy_stored_tokens: Option<u64>,
    pub(crate) expected_codex_cli_version: Option<&'a str>,
    pub(crate) expected_codex_catalog_digest: Option<&'a str>,
    pub(crate) observed_at: Option<DateTime<Utc>>,
}

impl<'a> ContextBudgetRequest<'a> {
    pub(crate) fn new(provider: SessionProvider, model: &'a str) -> Self {
        Self {
            provider,
            model,
            configured_tokens: None,
            runtime_effective_tokens: None,
            legacy_stored_tokens: None,
            expected_codex_cli_version: None,
            expected_codex_catalog_digest: None,
            observed_at: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ProviderCapabilityRegistry {
    codex_catalog: CodexCatalogCache,
}

impl ProviderCapabilityRegistry {
    pub(crate) fn refresh_codex_catalog(
        &self,
        cli_version: &str,
        raw: &[u8],
        observed_at: DateTime<Utc>,
    ) -> Result<CodexCatalogRefresh> {
        self.codex_catalog
            .refresh_from_bytes(cli_version, raw, observed_at)
    }

    pub(crate) fn record_codex_probe_failure(
        &self,
        cli_version: Option<String>,
        raw: Option<&[u8]>,
    ) {
        self.codex_catalog
            .record_probe_failure(cli_version, raw.map(content_digest));
    }

    pub(crate) fn cached_codex_catalog(&self) -> Option<Arc<CodexCatalogSnapshot>> {
        self.codex_catalog.cached_snapshot()
    }

    pub(crate) fn current_codex_catalog_for_version(
        &self,
        cli_version: &str,
    ) -> Option<Arc<CodexCatalogSnapshot>> {
        self.codex_catalog
            .current_snapshot_for_exact_version(cli_version)
    }

    pub(crate) fn resolve_context_budget(
        &self,
        request: ContextBudgetRequest<'_>,
    ) -> ResolvedContextBudget {
        let native_codex = is_native_codex_provider(request.provider);
        let catalog_observation = native_codex
            .then(|| {
                self.codex_catalog.current_snapshot_for_observation(
                    request.expected_codex_cli_version,
                    request.expected_codex_catalog_digest,
                )
            })
            .flatten();
        let catalog = catalog_observation
            .as_deref()
            .filter(|snapshot| snapshot.trust == CodexCatalogTrust::Allowlisted);
        let catalog_model = catalog.and_then(|snapshot| snapshot.model(request.model));
        let mut capacity = catalog_model
            .map(|model| model.capacity.clone())
            .unwrap_or_default();
        apply_official_capacity(request.model, &mut capacity);

        // Configuration remains descriptive even when a stronger live runtime
        // observation selects the active denominator.
        capacity.configured_tokens = positive(request.configured_tokens);

        if let Some(runtime_tokens) = positive(request.runtime_effective_tokens) {
            capacity.runtime_effective_tokens = Some(runtime_tokens);
            let pinned_catalog_identity = request
                .expected_codex_cli_version
                .zip(request.expected_codex_catalog_digest);
            return resolved(
                runtime_tokens,
                capacity,
                CapabilityEvidence {
                    source: CapabilitySource::RuntimeTelemetry,
                    source_version: pinned_catalog_identity
                        .map(|(version, _)| version.to_string())
                        .or_else(|| {
                            catalog_observation
                                .as_deref()
                                .map(|snapshot| snapshot.key.cli_version.clone())
                        }),
                    source_digest: pinned_catalog_identity
                        .map(|(_, digest)| digest.to_string())
                        .or_else(|| {
                            catalog_observation
                                .as_deref()
                                .map(|snapshot| snapshot.key.content_digest.to_string())
                        }),
                    observed_at: request.observed_at,
                    confidence: CapabilityConfidence::Authoritative,
                },
            );
        }

        // A native Codex configured value is the provider's raw input, not
        // necessarily its effective active window. Only an allowlisted
        // catalog may supply the percentage needed to promote that value to
        // Configured/Authoritative. Until then, retain the raw fact in
        // `capacity` and fall through to a degraded active-budget source.
        let configured_factor_is_trusted = !native_codex || capacity.effective_percent.is_some();
        if let Some(configured_tokens) = positive(request.configured_tokens)
            && configured_factor_is_trusted
        {
            let active_tokens =
                apply_effective_percent(configured_tokens, capacity.effective_percent)
                    .unwrap_or(configured_tokens);
            return resolved(
                active_tokens,
                capacity,
                CapabilityEvidence {
                    source: CapabilitySource::Configured,
                    source_version: catalog_observation
                        .as_deref()
                        .map(|snapshot| snapshot.key.cli_version.clone()),
                    source_digest: catalog_observation
                        .as_deref()
                        .map(|snapshot| snapshot.key.content_digest.to_string()),
                    observed_at: request.observed_at,
                    confidence: CapabilityConfidence::Authoritative,
                },
            );
        }

        if let (Some(snapshot), Some(model)) = (catalog, catalog_model)
            && let Some(active_tokens) = model.capacity.provider_effective_default_tokens()
        {
            return resolved(
                active_tokens,
                capacity,
                CapabilityEvidence {
                    source: CapabilitySource::ProviderCatalog,
                    source_version: Some(snapshot.key.cli_version.clone()),
                    source_digest: Some(snapshot.key.content_digest.to_string()),
                    observed_at: Some(snapshot.observed_at),
                    confidence: CapabilityConfidence::Verified,
                },
            );
        }

        let repository_tokens =
            repository_context_window_for_provider(request.provider, request.model);
        let ignores_legacy_stored = uses_codex_transport_fallback(request.provider);
        let legacy_tokens = (!ignores_legacy_stored)
            .then(|| positive(request.legacy_stored_tokens))
            .flatten();
        let active_tokens = match (repository_tokens, legacy_tokens) {
            (Some(repository), Some(legacy)) => repository.max(legacy),
            (Some(repository), None) => repository,
            (None, Some(legacy)) => legacy.max(DEFAULT_CONTEXT_WINDOW_TOKENS),
            (None, None) => DEFAULT_CONTEXT_WINDOW_TOKENS,
        };
        let stale_codex_evidence = native_codex
            && self.codex_catalog.has_unusable_capability_observation(
                request.expected_codex_cli_version,
                request.expected_codex_catalog_digest,
            );
        let legacy_selected = stale_codex_evidence
            || repository_tokens.is_none()
            || legacy_tokens.is_some_and(|legacy| {
                repository_tokens.is_none_or(|repository| legacy > repository)
            });
        let evidence = if legacy_selected {
            catalog_observation.map_or_else(CapabilityEvidence::legacy_unverified, |snapshot| {
                CapabilityEvidence {
                    source: CapabilitySource::LegacyUnverified,
                    source_version: Some(snapshot.key.cli_version.clone()),
                    source_digest: Some(snapshot.key.content_digest.to_string()),
                    observed_at: Some(snapshot.observed_at),
                    confidence: CapabilityConfidence::Degraded,
                }
            })
        } else {
            CapabilityEvidence {
                source: CapabilitySource::RepositoryFallback,
                source_version: Some(REPOSITORY_FALLBACK_VERSION.to_string()),
                source_digest: None,
                observed_at: None,
                confidence: CapabilityConfidence::Degraded,
            }
        };
        resolved(active_tokens, capacity, evidence)
    }
}

static PROVIDER_CAPABILITIES: LazyLock<ProviderCapabilityRegistry> =
    LazyLock::new(ProviderCapabilityRegistry::default);

pub(crate) fn provider_capabilities() -> &'static ProviderCapabilityRegistry {
    &PROVIDER_CAPABILITIES
}

#[cfg(test)]
struct PinnedCodexCatalogObservationForTest {
    runtime_config_key: usize,
    cli_version: String,
    raw_catalog: Vec<u8>,
    resolver_registry: Arc<ProviderCapabilityRegistry>,
}

#[cfg(test)]
static PINNED_CODEX_CATALOG_OBSERVATIONS_FOR_TEST: LazyLock<
    Mutex<std::collections::HashMap<uuid::Uuid, PinnedCodexCatalogObservationForTest>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Pin both the installed-catalog observation and the resolver's catalog
/// cache to one fixture session. The per-session resolver registry prevents
/// concurrent tests from changing the effective percentage after refresh.
#[cfg(test)]
pub fn pin_codex_catalog_observation_for_test(
    session_id: uuid::Uuid,
    runtime_config: &Arc<crate::config::RuntimeConfig>,
    cli_version: &str,
    raw_catalog: &[u8],
) -> Result<PinnedCodexCatalogObservationGuardForTest> {
    let resolver_registry = Arc::new(ProviderCapabilityRegistry::default());
    resolver_registry.refresh_codex_catalog(cli_version, raw_catalog, Utc::now())?;
    let runtime_config_key = Arc::as_ptr(runtime_config) as usize;
    PINNED_CODEX_CATALOG_OBSERVATIONS_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            session_id,
            PinnedCodexCatalogObservationForTest {
                runtime_config_key,
                cli_version: cli_version.to_string(),
                raw_catalog: raw_catalog.to_vec(),
                resolver_registry,
            },
        );
    Ok(PinnedCodexCatalogObservationGuardForTest { session_id })
}

#[cfg(test)]
pub struct PinnedCodexCatalogObservationGuardForTest {
    session_id: uuid::Uuid,
}

#[cfg(test)]
impl Drop for PinnedCodexCatalogObservationGuardForTest {
    fn drop(&mut self) {
        PINNED_CODEX_CATALOG_OBSERVATIONS_FOR_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

#[cfg(test)]
fn pinned_codex_catalog_registry_for_test(
    session_id: uuid::Uuid,
) -> Option<Arc<ProviderCapabilityRegistry>> {
    PINNED_CODEX_CATALOG_OBSERVATIONS_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&session_id)
        .map(|observation| Arc::clone(&observation.resolver_registry))
}

#[cfg(test)]
fn pinned_codex_catalog_observation_for_runtime_config(
    runtime_config: &Arc<crate::config::RuntimeConfig>,
) -> Option<(String, Vec<u8>)> {
    let runtime_config_key = Arc::as_ptr(runtime_config) as usize;
    PINNED_CODEX_CATALOG_OBSERVATIONS_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .find(|observation| observation.runtime_config_key == runtime_config_key)
        .map(|observation| {
            (
                observation.cli_version.clone(),
                observation.raw_catalog.clone(),
            )
        })
}

/// Resolve a new provider-process incarnation after invalidating any prior
/// runtime telemetry authority.
pub(crate) fn resolve_fresh_context_budget(
    provider: SessionProvider,
    model: &str,
    configured_tokens: Option<u64>,
) -> ResolvedContextBudget {
    resolve_fresh_context_budget_with_registry(
        provider_capabilities(),
        provider,
        model,
        configured_tokens,
    )
}

fn resolve_fresh_context_budget_with_registry(
    registry: &ProviderCapabilityRegistry,
    provider: SessionProvider,
    model: &str,
    configured_tokens: Option<u64>,
) -> ResolvedContextBudget {
    let mut request = ContextBudgetRequest::new(provider, model);
    request.configured_tokens = configured_tokens;
    registry.resolve_context_budget(request)
}

/// Resolve a replacement provider-process incarnation. Runtime telemetry is
/// intentionally discarded. A configured authority is recomputed when its raw
/// configured value is still available; older C3 rows retain their exact
/// configured active value because the migration did not persist that raw
/// descriptive field.
pub(crate) fn resolve_new_incarnation_context_budget(session: &Session) -> ResolvedContextBudget {
    #[cfg(test)]
    if let Some(registry) = pinned_codex_catalog_registry_for_test(session.id) {
        return resolve_new_incarnation_context_budget_for_with_registry(
            &registry,
            session.provider,
            session.model.as_deref().unwrap_or("unknown"),
            session.resolved_context_budget.as_ref(),
        );
    }

    resolve_new_incarnation_context_budget_for(
        session.provider,
        session.model.as_deref().unwrap_or("unknown"),
        session.resolved_context_budget.as_ref(),
    )
}

pub(crate) fn resolve_new_incarnation_context_budget_for(
    provider: SessionProvider,
    model: &str,
    existing: Option<&ResolvedContextBudget>,
) -> ResolvedContextBudget {
    resolve_new_incarnation_context_budget_for_with_registry(
        provider_capabilities(),
        provider,
        model,
        existing,
    )
}

fn resolve_new_incarnation_context_budget_for_with_registry(
    registry: &ProviderCapabilityRegistry,
    provider: SessionProvider,
    model: &str,
    existing: Option<&ResolvedContextBudget>,
) -> ResolvedContextBudget {
    if let Some(existing) = existing
        && existing.evidence.source == CapabilitySource::Configured
        && existing.capacity.configured_tokens.is_none()
    {
        return rehydrate_resolved_context_budget(provider, model, existing.clone());
    }
    let configured_tokens = existing.and_then(|budget| budget.capacity.configured_tokens);
    resolve_fresh_context_budget_with_registry(registry, provider, model, configured_tokens)
}

/// Resolve the semantic budget for an already materialized Session.
///
/// Persisted provenance wins. Rows predating provenance retain their scalar as
/// legacy-unverified evidence; repository/catalog facts cannot silently
/// replace it on read.
pub(crate) fn resolved_context_budget_for_session(session: &Session) -> ResolvedContextBudget {
    if let Some(resolved) = session.resolved_context_budget.clone() {
        return rehydrate_resolved_context_budget(
            session.provider,
            session.model.as_deref().unwrap_or("unknown"),
            resolved,
        );
    }

    let mut request = ContextBudgetRequest::new(
        session.provider,
        session.model.as_deref().unwrap_or("unknown"),
    );
    request.legacy_stored_tokens = session.context_window;
    provider_capabilities().resolve_context_budget(request)
}

/// Reattach currently validated descriptive capacity without altering the
/// persisted active denominator or its provenance.
pub(crate) fn rehydrate_resolved_context_budget(
    provider: SessionProvider,
    model: &str,
    mut resolved: ResolvedContextBudget,
) -> ResolvedContextBudget {
    let prior_capacity = resolved.capacity.clone();
    let mut capacity = ContextCapacity::default();

    if is_native_codex_provider(provider)
        && let (Some(version), Some(digest)) = (
            resolved.evidence.source_version.as_deref(),
            resolved.evidence.source_digest.as_deref(),
        )
        && let Some(snapshot) = provider_capabilities()
            .codex_catalog
            .current_allowlisted_snapshot_for_exact_version(version)
        && snapshot.key.content_digest.as_str() == digest
        && let Some(catalog_model) = snapshot.model(model)
    {
        capacity = catalog_model.capacity.clone();
    }
    apply_official_capacity(model, &mut capacity);

    // Preserve facts that are specific to the persisted/in-memory session;
    // catalog enrichment only fills the provider envelope around them.
    capacity.configured_tokens = prior_capacity.configured_tokens;
    capacity.runtime_effective_tokens = prior_capacity.runtime_effective_tokens;
    capacity.compaction_limit_tokens = prior_capacity.compaction_limit_tokens;
    if resolved.evidence.source == CapabilitySource::RuntimeTelemetry {
        capacity.runtime_effective_tokens = Some(resolved.active_tokens);
    }
    resolved.capacity = capacity;
    resolved
}

/// Refresh the exact installed Codex catalog at a provider-incarnation
/// boundary. Failures poison only catalog freshness; the last parsed cache
/// remains available for legacy projection while resolution degrades.
pub(crate) async fn refresh_installed_catalog_for_provider(
    provider: SessionProvider,
    runtime_config: Arc<crate::config::RuntimeConfig>,
) -> Result<()> {
    #[cfg(test)]
    if pinned_codex_catalog_observation_for_runtime_config(&runtime_config).is_some() {
        // The pinned per-session registry was parsed and validated during
        // fixture setup. Treat that observation as the installed CLI result
        // without invoking the host binary or mutating the shared cache.
        return Ok(());
    }

    if !is_native_codex_provider(provider) {
        return Ok(());
    }
    let client = match crate::codex::CodexClient::new(runtime_config) {
        Ok(client) => client,
        Err(error) => {
            provider_capabilities().record_codex_probe_failure(None, None);
            return Err(error);
        }
    };
    client
        .refresh_catalog(provider_capabilities(), CatalogRefreshReason::VersionChange)
        .await?;
    Ok(())
}

fn repository_context_window_for_provider(provider: SessionProvider, model: &str) -> Option<u64> {
    let model = model.trim();
    let normalized_model = if provider == SessionProvider::Claude {
        rsi_common::claude_catalog::strip_context_variant_suffix(model)
    } else {
        model
    };
    let normalized = normalized_model.to_ascii_lowercase();
    if uses_codex_transport_fallback(provider)
        && let Some((_, tokens)) = CODEX_TRANSPORT_FALLBACKS
            .iter()
            .find(|(pattern, _)| normalized.starts_with(pattern))
    {
        return Some(*tokens);
    }
    if provider == SessionProvider::Claude {
        if let Some(tokens) =
            rsi_common::claude_catalog::claude_catalog_context_window(normalized_model)
        {
            return Some(tokens);
        }
        if let Some((_, tokens)) = RETIRED_CLAUDE_MODEL_WINDOWS
            .iter()
            .find(|(id, _)| *id == normalized)
        {
            return Some(*tokens);
        }
    }
    repository_context_window_normalized(&normalized)
}

fn repository_context_window_normalized(normalized: &str) -> Option<u64> {
    // Preserve the historical exact match: version suffixes on Opus 5 are not
    // assumed to share a capacity until provider evidence says so.
    if normalized == "claude-opus-5" {
        return Some(1_000_000);
    }
    REPOSITORY_MODEL_FALLBACKS
        .iter()
        .find(|(pattern, _)| normalized.contains(pattern))
        .map(|(_, tokens)| *tokens)
}

fn is_native_codex_provider(provider: SessionProvider) -> bool {
    matches!(
        provider,
        SessionProvider::Codex | SessionProvider::CodexAppServer
    )
}

fn uses_codex_transport_fallback(provider: SessionProvider) -> bool {
    matches!(
        provider,
        SessionProvider::Codex
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock
            | SessionProvider::CodexAppServer
    )
}

fn positive(value: Option<u64>) -> Option<u64> {
    value.filter(|value| *value > 0)
}

fn canonical_capability_model_slug(model: &str) -> &str {
    model.trim()
}

fn apply_official_capacity(model: &str, capacity: &mut ContextCapacity) {
    let model = canonical_capability_model_slug(model);
    let Some(official) = OFFICIAL_MODEL_CAPACITIES
        .iter()
        .find(|entry| entry.slug.eq_ignore_ascii_case(model))
    else {
        return;
    };
    capacity.advertised_max_tokens = Some(official.advertised_max_tokens);
    capacity.max_output_tokens = Some(official.max_output_tokens);
}

fn apply_effective_percent(tokens: u64, percent: Option<u8>) -> Option<u64> {
    tokens
        .checked_mul(u64::from(percent.unwrap_or(100)))
        .map(|scaled| scaled / 100)
}

fn resolved(
    active_tokens: u64,
    capacity: ContextCapacity,
    evidence: CapabilityEvidence,
) -> ResolvedContextBudget {
    ResolvedContextBudget::new(active_tokens, capacity, evidence)
        .expect("provider capability resolver always emits a positive budget")
}

fn content_digest(raw: &[u8]) -> Sha256Digest {
    let digest = Sha256::digest(raw);
    Sha256Digest::parse(format!("sha256:{}", hex::encode(digest)))
        .expect("SHA-256 construction is canonical")
}

fn codex_catalog_trust(key: &CodexCatalogCacheKey) -> CodexCatalogTrust {
    if VALIDATED_CODEX_CATALOG_CONTRACTS
        .iter()
        .any(|(version, digest)| {
            *version == key.cli_version.as_str() && *digest == key.content_digest.as_str()
        })
    {
        CodexCatalogTrust::Allowlisted
    } else {
        CodexCatalogTrust::DiscoveryOnly
    }
}

#[derive(Debug, Deserialize)]
struct CodexModelCatalogWire {
    models: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct CodexCatalogModelWire {
    slug: String,
    display_name: Option<String>,
    description: Option<String>,
    visibility: Option<String>,
    priority: Option<i64>,
    supported_in_api: Option<bool>,
    context_window: Option<u64>,
    max_context_window: Option<u64>,
    effective_context_window_percent: Option<u8>,
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningLevelWire>,
    #[serde(default)]
    input_modalities: Vec<String>,
    apply_patch_tool_type: Option<String>,
    #[serde(default)]
    experimental_supported_tools: Vec<Value>,
    shell_type: Option<String>,
    supports_image_detail_original: Option<bool>,
    supports_search_tool: Option<bool>,
    tool_mode: Option<String>,
    web_search_tool_type: Option<String>,
    comp_hash: Option<String>,
    truncation_policy: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CodexReasoningLevelWire {
    effort: String,
    description: Option<String>,
}

pub(crate) fn parse_codex_catalog_snapshot(
    cli_version: &str,
    raw: &[u8],
    observed_at: DateTime<Utc>,
) -> Result<CodexCatalogSnapshot> {
    let cli_version = cli_version.trim();
    if cli_version.is_empty() {
        return Err(DaemonError::Process(
            "Codex CLI catalog evidence requires a non-empty version".to_string(),
        ));
    }
    let root: Value = serde_json::from_slice(raw)?;
    let raw_keys = root
        .as_object()
        .ok_or_else(|| DaemonError::Process("Codex model catalog must be an object".to_string()))?
        .keys()
        .cloned()
        .collect();
    let wire: CodexModelCatalogWire = serde_json::from_value(root)?;
    let models = wire
        .models
        .into_iter()
        .map(parse_codex_catalog_model)
        .collect::<Result<Vec<_>>>()?;
    let mut seen_models = HashSet::new();
    for model in &models {
        let normalized = model.slug.trim().to_ascii_lowercase();
        if !seen_models.insert(normalized) {
            return Err(DaemonError::Process(format!(
                "Codex catalog repeats model slug {}",
                model.slug
            )));
        }
    }

    let key = CodexCatalogCacheKey {
        cli_version: cli_version.to_string(),
        content_digest: content_digest(raw),
    };
    let trust = codex_catalog_trust(&key);

    Ok(CodexCatalogSnapshot {
        key,
        trust,
        observed_at,
        models,
        raw_keys,
    })
}

fn parse_codex_catalog_model(raw: Value) -> Result<CodexCatalogModel> {
    let raw_keys = raw
        .as_object()
        .ok_or_else(|| DaemonError::Process("Codex catalog model must be an object".to_string()))?
        .keys()
        .cloned()
        .collect();
    let wire: CodexCatalogModelWire = serde_json::from_value(raw)?;
    let slug = wire.slug;
    if slug.trim().is_empty() {
        return Err(DaemonError::Process(
            "Codex catalog model slug must not be empty".to_string(),
        ));
    }
    validate_positive_catalog_field(&slug, "context_window", wire.context_window)?;
    validate_positive_catalog_field(&slug, "max_context_window", wire.max_context_window)?;
    if let (Some(default), Some(maximum)) = (wire.context_window, wire.max_context_window)
        && default > maximum
    {
        return Err(DaemonError::Process(format!(
            "Codex catalog model {slug} context_window exceeds max_context_window"
        )));
    }
    if wire
        .effective_context_window_percent
        .is_some_and(|percent| !(1..=100).contains(&percent))
    {
        return Err(DaemonError::Process(format!(
            "Codex catalog model {slug} effective_context_window_percent must be in 1..=100"
        )));
    }
    if let (Some(default), Some(percent)) =
        (wire.context_window, wire.effective_context_window_percent)
    {
        default.checked_mul(u64::from(percent)).ok_or_else(|| {
            DaemonError::Process(format!(
                "Codex catalog model {slug} effective context calculation overflowed"
            ))
        })?;
    }

    let default_reasoning_level = match wire.default_reasoning_level {
        Some(level) if level.trim().is_empty() => {
            return Err(DaemonError::Process(format!(
                "Codex catalog model {slug} has an empty default reasoning effort"
            )));
        }
        Some(level) => Some(level.trim().to_string()),
        None => None,
    };
    let mut seen_efforts = HashSet::new();
    let supported_reasoning_levels = wire
        .supported_reasoning_levels
        .into_iter()
        .map(|level| {
            let effort = level.effort.trim().to_string();
            if effort.is_empty() {
                return Err(DaemonError::Process(format!(
                    "Codex catalog model {slug} has an empty reasoning effort"
                )));
            }
            if !seen_efforts.insert(effort.clone()) {
                return Err(DaemonError::Process(format!(
                    "Codex catalog model {slug} repeats reasoning effort {effort}"
                )));
            }
            Ok(CodexReasoningLevel {
                effort,
                description: level.description,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(default) = default_reasoning_level.as_deref()
        && !supported_reasoning_levels.is_empty()
        && !seen_efforts.contains(default)
    {
        return Err(DaemonError::Process(format!(
            "Codex catalog model {slug} default reasoning effort is not supported"
        )));
    }

    Ok(CodexCatalogModel {
        slug,
        display_name: wire.display_name,
        description: wire.description,
        visibility: wire.visibility,
        priority: wire.priority,
        supported_in_api: wire.supported_in_api,
        capacity: ContextCapacity {
            provider_default_tokens: wire.context_window,
            provider_max_tokens: wire.max_context_window,
            effective_percent: wire.effective_context_window_percent,
            ..ContextCapacity::default()
        },
        default_reasoning_level,
        supported_reasoning_levels,
        tools: CodexCatalogToolCapabilities {
            input_modalities: wire.input_modalities,
            apply_patch_tool_type: wire.apply_patch_tool_type,
            experimental_supported_tools: wire.experimental_supported_tools,
            shell_type: wire.shell_type,
            supports_image_detail_original: wire.supports_image_detail_original,
            supports_search_tool: wire.supports_search_tool,
            tool_mode: wire.tool_mode,
            web_search_tool_type: wire.web_search_tool_type,
        },
        compaction: CodexCatalogCompactionMetadata {
            comp_hash: wire.comp_hash,
            truncation_policy: wire.truncation_policy,
        },
        raw_keys,
    })
}

fn validate_positive_catalog_field(slug: &str, field: &str, value: Option<u64>) -> Result<()> {
    if value == Some(0) {
        return Err(DaemonError::Process(format!(
            "Codex catalog model {slug} {field} must be positive"
        )));
    }
    if value.is_some_and(|value| value > MAX_VALIDATED_CONTEXT_TOKENS) {
        return Err(DaemonError::Process(format!(
            "Codex catalog model {slug} {field} exceeds validated bound {MAX_VALIDATED_CONTEXT_TOKENS}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_0_155_1_FIXTURE: &[u8] =
        include_bytes!("../tests/fixtures/codex-models-0.155.1.json");
    const CODEX_0_155_1_VERSION: &str = "codex-cli 0.155.1";
    const CODEX_0_155_1_FIXTURE_DIGEST: &str = VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST;

    fn fixture_snapshot() -> CodexCatalogSnapshot {
        parse_codex_catalog_snapshot(
            CODEX_0_155_1_VERSION,
            CODEX_0_155_1_FIXTURE,
            DateTime::parse_from_rfc3339("2026-09-03T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap()
    }

    #[test]
    fn real_codex_0_155_1_fixture_preserves_capacity_reasoning_and_projection() {
        let snapshot = fixture_snapshot();
        assert_eq!(snapshot.key.cli_version, CODEX_0_155_1_VERSION);
        assert_eq!(snapshot.trust, CodexCatalogTrust::Allowlisted);
        assert_eq!(
            snapshot.key.content_digest.as_str(),
            CODEX_0_155_1_FIXTURE_DIGEST
        );
        assert!(snapshot.raw_keys.contains("models"));
        assert_eq!(
            snapshot.legacy_model_tuples(),
            vec![("gpt-6-astra".to_string(), "GPT-6-Astra".to_string())]
        );

        for model in &snapshot.models {
            assert_eq!(model.capacity.provider_default_tokens, Some(272_000));
            assert_eq!(model.capacity.provider_max_tokens, Some(872_000));
            assert_eq!(model.capacity.effective_percent, Some(95));
            assert_eq!(
                model.capacity.provider_effective_default_tokens(),
                Some(258_400)
            );
            assert!(model.raw_keys.contains("tool_mode"));
            assert!(model.raw_keys.contains("context_window"));
            assert!(model.raw_keys.contains("max_context_window"));
            assert!(model.raw_keys.contains("effective_context_window_percent"));
            assert!(model.raw_keys.contains("supported_reasoning_levels"));
            assert_eq!(model.tools.input_modalities, ["text", "image"]);
            assert_eq!(
                model.tools.apply_patch_tool_type.as_deref(),
                Some("freeform")
            );
            assert_eq!(model.tools.tool_mode.as_deref(), Some("code_mode_only"));
            assert_eq!(model.tools.supports_search_tool, Some(true));
            assert_eq!(
                model.tools.web_search_tool_type.as_deref(),
                Some("text_and_image")
            );
            assert_eq!(model.compaction.comp_hash.as_deref(), Some("3000"));
            assert_eq!(
                model.compaction.truncation_policy,
                Some(serde_json::json!({"mode": "tokens", "limit": 10_000}))
            );
            assert_eq!(model.capacity.max_output_tokens, None);
            assert_eq!(model.capacity.compaction_limit_tokens, None);
        }
        assert_eq!(
            snapshot.models[0].default_reasoning_level.as_deref(),
            Some("low")
        );
        assert_eq!(
            snapshot.models[0]
                .supported_reasoning_levels
                .iter()
                .map(|level| level.effort.as_str())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max", "ultra"]
        );
    }

    #[test]
    fn reviewed_raw_and_semantic_0_155_1_contracts_are_allowlisted_exactly() {
        for digest in [
            VALIDATED_CODEX_RAW_CATALOG_DIGEST,
            VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST,
        ] {
            let key = CodexCatalogCacheKey {
                cli_version: VALIDATED_CODEX_CLI_VERSION.to_string(),
                content_digest: Sha256Digest::parse(digest).unwrap(),
            };
            assert_eq!(codex_catalog_trust(&key), CodexCatalogTrust::Allowlisted);

            let future_key = CodexCatalogCacheKey {
                cli_version: "codex-cli 0.153.0".to_string(),
                content_digest: key.content_digest,
            };
            assert_eq!(
                codex_catalog_trust(&future_key),
                CodexCatalogTrust::DiscoveryOnly
            );
        }
    }

    #[test]
    fn catalog_rejects_zero_overflow_and_bad_percent() {
        for raw in [
            r#"{"models":[{"slug":"bad","context_window":0}]}"#,
            r#"{"models":[{"slug":"bad","max_context_window":0}]}"#,
            r#"{"models":[{"slug":"bad","context_window":18446744073709551616}]}"#,
            r#"{"models":[{"slug":"bad","context_window":18446744073709551615,"effective_context_window_percent":100}]}"#,
            r#"{"models":[{"slug":"bad","context_window":10000001}]}"#,
            r#"{"models":[{"slug":"bad","effective_context_window_percent":0}]}"#,
            r#"{"models":[{"slug":"bad","effective_context_window_percent":101}]}"#,
            r#"{"models":[{"slug":"bad","default_reasoning_level":" "}]}"#,
            r#"{"models":[{"slug":"bad","default_reasoning_level":"high","supported_reasoning_levels":[{"effort":"low"}]}]}"#,
            r#"{"models":[{"slug":"bad","supported_reasoning_levels":[{"effort":"low"},{"effort":"low"}]}]}"#,
            r#"{"models":[{"slug":"bad"},{"slug":"BAD"}]}"#,
        ] {
            assert!(
                parse_codex_catalog_snapshot(CODEX_0_155_1_VERSION, raw.as_bytes(), Utc::now())
                    .is_err(),
                "invalid catalog must fail: {raw}"
            );
        }
    }

    #[test]
    fn catalog_cache_reuses_exact_key_and_refreshes_version_or_digest() {
        let cache = CodexCatalogCache::default();
        let observed_at = Utc::now();
        let first = cache
            .refresh_from_bytes(CODEX_0_155_1_VERSION, CODEX_0_155_1_FIXTURE, observed_at)
            .unwrap();
        assert!(matches!(first, CodexCatalogRefresh::Updated(_)));
        let reused = cache
            .refresh_from_bytes(CODEX_0_155_1_VERSION, CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();
        assert!(matches!(reused, CodexCatalogRefresh::Reused(_)));
        assert!(Arc::ptr_eq(first.snapshot(), reused.snapshot()));
        assert!(
            cache
                .current_snapshot_for_exact_version(CODEX_0_155_1_VERSION)
                .is_some()
        );
        assert!(
            cache
                .current_snapshot_for_exact_version("codex-cli 0.153.0")
                .is_none()
        );

        let changed_version = cache
            .refresh_from_bytes("codex-cli 0.153.0", CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();
        assert!(matches!(changed_version, CodexCatalogRefresh::Updated(_)));
        assert_eq!(
            changed_version.snapshot().key.cli_version,
            "codex-cli 0.153.0"
        );
        assert_eq!(
            changed_version.snapshot().trust,
            CodexCatalogTrust::DiscoveryOnly
        );
        assert!(
            cache
                .current_snapshot_for_exact_version("codex-cli 0.153.0")
                .is_some()
        );
        assert!(
            cache
                .current_allowlisted_snapshot_for_exact_version("codex-cli 0.153.0")
                .is_none()
        );

        let changed_raw = String::from_utf8(CODEX_0_155_1_FIXTURE.to_vec())
            .unwrap()
            .replace(
                "Our most capable model for complex, demanding work.",
                "Changed catalog description.",
            );
        let changed_digest = cache
            .refresh_from_bytes("codex-cli 0.153.0", changed_raw.as_bytes(), Utc::now())
            .unwrap();
        assert!(matches!(changed_digest, CodexCatalogRefresh::Updated(_)));
        assert_ne!(
            changed_version.snapshot().key.content_digest,
            changed_digest.snapshot().key.content_digest
        );
    }

    #[test]
    fn unreviewed_version_or_digest_stays_discoverable_but_degrades_capacity() {
        let registry = ProviderCapabilityRegistry::default();
        let future_version = "codex-cli 0.153.0";
        let future = registry
            .refresh_codex_catalog(future_version, CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();
        assert_eq!(future.snapshot().trust, CodexCatalogTrust::DiscoveryOnly);
        assert_eq!(
            future.snapshot().legacy_model_tuples(),
            fixture_snapshot().legacy_model_tuples()
        );

        let mut request = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        request.expected_codex_cli_version = Some(future_version);
        let future_budget = registry.resolve_context_budget(request);
        assert_eq!(future_budget.active_tokens, CODEX_EFFECTIVE_FALLBACK_TOKENS);
        assert_eq!(future_budget.capacity.provider_default_tokens, None);
        assert_eq!(
            future_budget.evidence.source,
            CapabilitySource::LegacyUnverified
        );
        assert_eq!(
            future_budget.evidence.confidence,
            CapabilityConfidence::Degraded
        );
        assert_eq!(
            future_budget.evidence.source_version.as_deref(),
            Some(future_version)
        );
        assert_eq!(
            future_budget.evidence.source_digest.as_deref(),
            Some(future.snapshot().key.content_digest.as_str())
        );

        let changed_raw = String::from_utf8(CODEX_0_155_1_FIXTURE.to_vec())
            .unwrap()
            .replace(
                "Our most capable model for complex, demanding work.",
                "Changed catalog description.",
            );
        let changed_digest = registry
            .refresh_codex_catalog(CODEX_0_155_1_VERSION, changed_raw.as_bytes(), Utc::now())
            .unwrap();
        assert_eq!(
            changed_digest.snapshot().trust,
            CodexCatalogTrust::DiscoveryOnly
        );

        let mut request = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        request.expected_codex_cli_version = Some(CODEX_0_155_1_VERSION);
        let changed_budget = registry.resolve_context_budget(request);
        assert_eq!(
            changed_budget.active_tokens,
            CODEX_EFFECTIVE_FALLBACK_TOKENS
        );
        assert_eq!(changed_budget.capacity.provider_default_tokens, None);
        assert_eq!(
            changed_budget.evidence.source,
            CapabilitySource::LegacyUnverified
        );
        assert_eq!(
            changed_budget.evidence.source_version.as_deref(),
            Some(CODEX_0_155_1_VERSION)
        );
        assert_eq!(
            changed_budget.evidence.source_digest.as_deref(),
            Some(changed_digest.snapshot().key.content_digest.as_str())
        );

        let mut runtime_request = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        runtime_request.expected_codex_cli_version = Some(CODEX_0_155_1_VERSION);
        runtime_request.runtime_effective_tokens = Some(300_000);
        let runtime = registry.resolve_context_budget(runtime_request);
        assert_eq!(runtime.active_tokens, 300_000);
        assert_eq!(runtime.capacity.provider_default_tokens, None);
        assert_eq!(runtime.evidence.source, CapabilitySource::RuntimeTelemetry);
        assert_eq!(
            runtime.evidence.confidence,
            CapabilityConfidence::Authoritative
        );
        assert_eq!(
            runtime.evidence.source_version.as_deref(),
            Some(CODEX_0_155_1_VERSION)
        );
        assert_eq!(
            runtime.evidence.source_digest.as_deref(),
            Some(changed_digest.snapshot().key.content_digest.as_str())
        );
    }

    #[test]
    fn discovery_only_launch_pins_catalog_identity_across_intervening_refresh() {
        let registry = ProviderCapabilityRegistry::default();
        let changed_raw = String::from_utf8(CODEX_0_155_1_FIXTURE.to_vec())
            .unwrap()
            .replace(
                "Our most capable model for complex, demanding work.",
                "Changed catalog description.",
            );
        let launch_snapshot = registry
            .refresh_codex_catalog(CODEX_0_155_1_VERSION, changed_raw.as_bytes(), Utc::now())
            .unwrap();
        assert_eq!(
            launch_snapshot.snapshot().trust,
            CodexCatalogTrust::DiscoveryOnly
        );

        let launch_budget = registry.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Codex,
            "gpt-6-astra",
        ));
        assert_eq!(
            launch_budget.evidence.source,
            CapabilitySource::LegacyUnverified
        );
        assert_eq!(launch_budget.capacity.provider_default_tokens, None);
        let launch_version = launch_budget
            .evidence
            .source_version
            .as_deref()
            .expect("discovery-only launch version is pinned");
        let launch_digest = launch_budget
            .evidence
            .source_digest
            .as_deref()
            .expect("discovery-only launch digest is pinned");
        assert_eq!(launch_version, CODEX_0_155_1_VERSION);
        assert_eq!(
            launch_digest,
            launch_snapshot.snapshot().key.content_digest.as_str()
        );

        let mut configured_request =
            ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        configured_request.configured_tokens = Some(400_000);
        let configured_budget = registry.resolve_context_budget(configured_request);
        assert_eq!(
            configured_budget.active_tokens,
            CODEX_EFFECTIVE_FALLBACK_TOKENS
        );
        assert_eq!(configured_budget.capacity.configured_tokens, Some(400_000));
        assert_eq!(
            configured_budget.evidence.source,
            CapabilitySource::LegacyUnverified
        );
        assert_eq!(
            configured_budget.evidence.confidence,
            CapabilityConfidence::Degraded
        );
        assert_eq!(
            configured_budget.evidence.source_version.as_deref(),
            Some(launch_version)
        );
        assert_eq!(
            configured_budget.evidence.source_digest.as_deref(),
            Some(launch_digest)
        );
        assert_eq!(configured_budget.capacity.provider_default_tokens, None);
        assert_eq!(configured_budget.capacity.provider_max_tokens, None);
        assert_eq!(configured_budget.capacity.effective_percent, None);
        assert!(!configured_budget.authorizes_threshold_rotation());

        let replacement = registry
            .refresh_codex_catalog(CODEX_0_155_1_VERSION, CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();
        assert_eq!(replacement.snapshot().trust, CodexCatalogTrust::Allowlisted);
        assert_ne!(
            launch_snapshot.snapshot().key.content_digest,
            replacement.snapshot().key.content_digest
        );

        let mut runtime_request = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        runtime_request.runtime_effective_tokens = Some(258_400);
        runtime_request.expected_codex_cli_version = Some(launch_version);
        runtime_request.expected_codex_catalog_digest = Some(launch_digest);
        runtime_request.observed_at = Some(Utc::now());
        let runtime = registry.resolve_context_budget(runtime_request);

        assert_eq!(runtime.active_tokens, 258_400);
        assert_eq!(runtime.evidence.source, CapabilitySource::RuntimeTelemetry);
        assert_eq!(
            runtime.evidence.source_version,
            launch_budget.evidence.source_version
        );
        assert_eq!(
            runtime.evidence.source_digest,
            launch_budget.evidence.source_digest
        );
        assert_eq!(
            runtime.capacity.provider_default_tokens, None,
            "replacement catalog capacity must not leak into the older incarnation"
        );
        assert_eq!(runtime.capacity.provider_max_tokens, None);
        assert_eq!(runtime.capacity.effective_percent, None);
    }

    #[test]
    fn failed_refresh_preserves_parsed_cache_but_degrades_resolution() {
        let registry = ProviderCapabilityRegistry::default();
        registry
            .refresh_codex_catalog(CODEX_0_155_1_VERSION, CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();
        let prior = registry.cached_codex_catalog().unwrap();
        assert!(
            registry
                .refresh_codex_catalog(CODEX_0_155_1_VERSION, b"{not-json", Utc::now())
                .is_err()
        );
        let preserved = registry.cached_codex_catalog().unwrap();
        assert!(Arc::ptr_eq(&prior, &preserved));

        let resolved = registry.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Codex,
            "gpt-6-astra",
        ));
        assert_eq!(resolved.active_tokens, CODEX_EFFECTIVE_FALLBACK_TOKENS);
        assert_eq!(resolved.evidence.source, CapabilitySource::LegacyUnverified);
        assert_eq!(resolved.evidence.confidence, CapabilityConfidence::Degraded);
    }

    #[test]
    fn claude_repository_resolution_uses_shared_catalog_and_variant_normalization() {
        let registry = ProviderCapabilityRegistry::default();
        for (model, expected) in [
            // Offered today: answered by the shared catalog.
            ("claude-fable-5-1", 1_000_000),
            // Retired from the catalog; answered by RETIRED_CLAUDE_MODEL_WINDOWS.
            ("claude-fable-5", 1_000_000),
            ("claude-opus-5[1m]", 1_000_000),
            ("claude-haiku-4-5-20251001[200k]", 200_000),
            ("claude-opus-4-7-200k[1m]", 200_000),
        ] {
            let resolved = registry
                .resolve_context_budget(ContextBudgetRequest::new(SessionProvider::Claude, model));
            assert_eq!(resolved.active_tokens, expected, "model {model}");
            assert_eq!(
                resolved.evidence.source,
                CapabilitySource::RepositoryFallback,
                "model {model}"
            );
            assert!(!resolved.authorizes_threshold_rotation());
        }

        let local = registry.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Local,
            "claude-fable-5",
        ));
        assert_eq!(local.active_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(local.evidence.source, CapabilitySource::LegacyUnverified);
    }

    #[test]
    fn resolver_distinguishes_catalog_repository_unknown_pioneer_and_authority() {
        let registry = ProviderCapabilityRegistry::default();
        registry
            .refresh_codex_catalog(CODEX_0_155_1_VERSION, CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();

        let mut catalog_request = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        catalog_request.expected_codex_cli_version = Some(CODEX_0_155_1_VERSION);
        let catalog = registry.resolve_context_budget(catalog_request);
        assert_eq!(catalog.active_tokens, 258_400);
        assert_eq!(catalog.capacity.advertised_max_tokens, Some(1_050_000));
        assert_eq!(catalog.capacity.max_output_tokens, Some(128_000));
        assert_eq!(catalog.evidence.source, CapabilitySource::ProviderCatalog);
        assert_eq!(catalog.evidence.confidence, CapabilityConfidence::Verified);
        assert_eq!(
            catalog.evidence.source_version.as_deref(),
            Some(CODEX_0_155_1_VERSION)
        );
        assert_eq!(
            catalog.evidence.source_digest.as_deref(),
            Some(CODEX_0_155_1_FIXTURE_DIGEST)
        );
        assert!(catalog.evidence.observed_at.is_some());
        assert!(!catalog.authorizes_threshold_rotation());

        let empty = ProviderCapabilityRegistry::default();
        let repository = empty.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Codex,
            "gpt-6-astra",
        ));
        assert_eq!(repository.active_tokens, CODEX_EFFECTIVE_FALLBACK_TOKENS);
        assert_eq!(
            repository.evidence.source,
            CapabilitySource::RepositoryFallback
        );
        assert!(!repository.authorizes_threshold_rotation());

        let unknown = empty.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Local,
            "not-a-known-model",
        ));
        assert_eq!(unknown.active_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert_eq!(unknown.evidence.source, CapabilitySource::LegacyUnverified);

        let pioneer = registry.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Pioneer,
            "gpt-6-astra",
        ));
        assert_eq!(pioneer.active_tokens, CODEX_EFFECTIVE_FALLBACK_TOKENS);
        assert_eq!(
            pioneer.evidence.source,
            CapabilitySource::RepositoryFallback
        );
        assert_eq!(pioneer.capacity.provider_default_tokens, None);

        let mut stale = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        stale.expected_codex_cli_version = Some("codex-cli 0.153.0");
        let stale = registry.resolve_context_budget(stale);
        assert_eq!(stale.evidence.source, CapabilitySource::LegacyUnverified);

        let mut configured = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        configured.configured_tokens = Some(400_000);
        let configured = registry.resolve_context_budget(configured);
        assert_eq!(configured.active_tokens, 380_000);
        assert_eq!(configured.capacity.configured_tokens, Some(400_000));
        assert_eq!(configured.capacity.effective_percent, Some(95));
        assert_eq!(
            configured.evidence.source_version.as_deref(),
            Some(CODEX_0_155_1_VERSION)
        );
        assert_eq!(
            configured.evidence.source_digest.as_deref(),
            Some(CODEX_0_155_1_FIXTURE_DIGEST)
        );
        assert!(configured.authorizes_threshold_rotation());

        let mut runtime = ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        runtime.configured_tokens = Some(400_000);
        runtime.runtime_effective_tokens = Some(258_400);
        let runtime = registry.resolve_context_budget(runtime);
        assert_eq!(runtime.active_tokens, 258_400);
        assert_eq!(runtime.evidence.source, CapabilitySource::RuntimeTelemetry);
        assert!(runtime.authorizes_threshold_rotation());

        let harness = registry.resolve_context_budget(ContextBudgetRequest::new(
            SessionProvider::Harness,
            "gpt-6-astra",
        ));
        assert_eq!(harness.active_tokens, 1_050_000);
        assert_eq!(harness.capacity.advertised_max_tokens, Some(1_050_000));
        assert_eq!(harness.capacity.max_output_tokens, Some(128_000));
        assert!(!harness.authorizes_threshold_rotation());
    }

    #[test]
    fn supported_gpt_6_astra_resolves_capacity_and_provenance() {
        let registry = ProviderCapabilityRegistry::default();
        registry
            .refresh_codex_catalog(CODEX_0_155_1_VERSION, CODEX_0_155_1_FIXTURE, Utc::now())
            .unwrap();

        let mut canonical_request =
            ContextBudgetRequest::new(SessionProvider::Codex, "gpt-6-astra");
        canonical_request.expected_codex_cli_version = Some(CODEX_0_155_1_VERSION);
        let canonical = registry.resolve_context_budget(canonical_request);

        assert_eq!(canonical.evidence.source, CapabilitySource::ProviderCatalog);
        assert_eq!(
            canonical.evidence.confidence,
            CapabilityConfidence::Verified
        );
        assert_eq!(canonical.capacity.advertised_max_tokens, Some(1_050_000));
        assert_eq!(canonical.capacity.provider_default_tokens, Some(272_000));
    }

    #[test]
    fn resume_and_retry_invalidate_runtime_but_preserve_configured_authority() {
        let runtime = ResolvedContextBudget::new(
            258_400,
            ContextCapacity {
                runtime_effective_tokens: Some(258_400),
                ..ContextCapacity::default()
            },
            CapabilityEvidence {
                source: CapabilitySource::RuntimeTelemetry,
                source_version: Some(CODEX_0_155_1_VERSION.to_string()),
                source_digest: Some(CODEX_0_155_1_FIXTURE_DIGEST.to_string()),
                observed_at: Some(Utc::now()),
                confidence: CapabilityConfidence::Authoritative,
            },
        )
        .unwrap();
        let resumed = resolve_new_incarnation_context_budget_for(
            SessionProvider::Codex,
            "gpt-6-astra",
            Some(&runtime),
        );
        assert_ne!(resumed.evidence.source, CapabilitySource::RuntimeTelemetry);
        assert_eq!(resumed.capacity.runtime_effective_tokens, None);
        assert!(!resumed.authorizes_threshold_rotation());

        let configured = ResolvedContextBudget::new(
            200_000,
            ContextCapacity {
                configured_tokens: Some(200_000),
                ..ContextCapacity::default()
            },
            CapabilityEvidence {
                source: CapabilitySource::Configured,
                source_version: None,
                source_digest: None,
                observed_at: Some(Utc::now()),
                confidence: CapabilityConfidence::Authoritative,
            },
        )
        .unwrap();
        let resumed = resolve_new_incarnation_context_budget_for(
            SessionProvider::Local,
            "not-a-known-model",
            Some(&configured),
        );
        assert_eq!(resumed.active_tokens, 200_000);
        assert_eq!(resumed.evidence.source, CapabilitySource::Configured);
        assert_eq!(resumed.capacity.configured_tokens, Some(200_000));
        assert!(resumed.authorizes_threshold_rotation());

        // C3 persisted only the resolved scalar and evidence. Preserve those
        // older configured rows exactly when the raw configured input cannot
        // be reconstructed during resume/retry.
        let c3_configured = ResolvedContextBudget::new(
            190_000,
            ContextCapacity::default(),
            CapabilityEvidence {
                source: CapabilitySource::Configured,
                source_version: None,
                source_digest: None,
                observed_at: Some(Utc::now()),
                confidence: CapabilityConfidence::Authoritative,
            },
        )
        .unwrap();
        let resumed = resolve_new_incarnation_context_budget_for(
            SessionProvider::Local,
            "not-a-known-model",
            Some(&c3_configured),
        );
        assert_eq!(resumed.active_tokens, 190_000);
        assert_eq!(resumed.evidence.source, CapabilitySource::Configured);
    }

    #[test]
    fn restart_rehydration_preserves_active_provenance_and_separates_maximum() {
        let persisted = ResolvedContextBudget::new(
            258_400,
            ContextCapacity::default(),
            CapabilityEvidence {
                source: CapabilitySource::RuntimeTelemetry,
                source_version: Some(CODEX_0_155_1_VERSION.to_string()),
                source_digest: Some(CODEX_0_155_1_FIXTURE_DIGEST.to_string()),
                observed_at: Some(Utc::now()),
                confidence: CapabilityConfidence::Authoritative,
            },
        )
        .unwrap();

        let rehydrated = rehydrate_resolved_context_budget(
            SessionProvider::Codex,
            "gpt-6-astra",
            persisted.clone(),
        );
        assert_eq!(rehydrated.active_tokens, persisted.active_tokens);
        assert_eq!(rehydrated.evidence, persisted.evidence);
        assert_eq!(rehydrated.capacity.runtime_effective_tokens, Some(258_400));
        assert_eq!(rehydrated.capacity.advertised_max_tokens, Some(1_050_000));
        assert_ne!(
            rehydrated.active_tokens,
            rehydrated.capacity.advertised_max_tokens.unwrap()
        );
    }

    #[test]
    fn legacy_consumers_have_no_provider_specific_mapping_or_stale_numeric_value() {
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let stale_spaced = ["372", "_000"].concat();
        let stale_plain = ["372", "000"].concat();
        let mut stale_hits = Vec::new();
        for entry in walkdir::WalkDir::new(&source_root)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("rs"))
        {
            let source = std::fs::read_to_string(entry.path()).unwrap();
            if source.contains(&stale_spaced) || source.contains(&stale_plain) {
                stale_hits.push(entry.path().to_path_buf());
            }
        }
        assert!(
            stale_hits.is_empty(),
            "stale context mappings: {stale_hits:?}"
        );

        let monitor = include_str!("monitor.rs");
        let session_types = include_str!("session/types.rs");
        assert!(!monitor.contains("static PATTERNS"));
        assert!(!session_types.contains("codex_cli_context_window_for_model"));
        assert!(!session_types.contains("context_window_for_provider_model"));
    }
}
