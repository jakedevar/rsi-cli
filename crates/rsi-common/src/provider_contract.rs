//! Typed CLI contract state of the provider CLIs (#2755): whether the installed
//! `claude` and `codex` still behave the way RSI's launch code assumes.
//!
//! RSI re-implements pieces of both CLIs' contracts by hand: the SHA-256 trust
//! identity of the Codex hooks that carry agent mail and the sandbox custody
//! guard (#2320), the ~14 flags a Claude launch passes, and the non-`--bare`
//! Claude behaviour (ambient OAuth login, hooks, CLAUDE.md). A CLI that drifts
//! breaks these silently: an untrusted Codex hook is merely not run. The daemon
//! therefore runs a bounded, non-billing *canary* per CLI at start and whenever
//! the CLI's version changes, and keeps exactly four facts per provider here:
//! the [`ProviderContractState`], a stable reason code, the CLI version and the
//! check time. Nothing else the CLIs print has a field.
//!
//! The operator methods ([`METHOD_GET`], [`METHOD_REFRESH`]) are operator-only:
//! they stay out of `AGENT_VERBS`, `READ_VERBS`, the native tools and the agent
//! CLI catalog. Agents read the same state through `AgentGetProviderStatus`
//! (`contract` on the `claude`, `codex` and `codex_app_server` entries).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Operator RPC: the contract state of every checked provider.
pub const METHOD_GET: &str = "GetProviderContractState";
/// Operator RPC: run the canary now (bypassing the version gate) and return
/// the new state.
pub const METHOD_REFRESH: &str = "RefreshProviderContractState";
/// Operator-only methods this module owns.
pub const OPERATOR_METHODS: [&str; 2] = [METHOD_GET, METHOD_REFRESH];

/// Daemon setting: run the canary at all (default on).
pub const SETTING_CANARY: &str = "provider_contract_canary";
/// Daemon setting: refuse Codex launches while the custody hook is untrusted
/// (default on, because the custody hook protects the sandbox branch).
pub const SETTING_REFUSE_UNTRUSTED_CUSTODY_HOOK: &str = "codex_refuse_untrusted_custody_hook";

/// Error code for an unknown or non-checked `provider` on a request.
pub const PROVIDER_CONTRACT_UNKNOWN_PROVIDER: &str = "provider_contract_unknown_provider";

/// The providers whose CLI contract is checked, in stable order.
pub const PROVIDER_CONTRACT_NAMES: [&str; 2] = ["claude", "codex"];

/// Codex reason: a Codex hook RSI installs is installed but not trusted.
pub const REASON_CODEX_HOOK_UNTRUSTED: &str = "codex_hook_untrusted";
/// Codex reason: Codex's hash of a hook differs from the hash RSI computes.
pub const REASON_CODEX_HOOK_HASH_MISMATCH: &str = "codex_hook_hash_mismatch";
/// Codex reason: a Codex hook RSI installs is not listed at all.
pub const REASON_CODEX_HOOK_MISSING: &str = "codex_hook_missing";
/// Codex reason (#2766): a lifecycle hook RSI can install (`PreCompact`) is
/// missing, untrusted or hashed differently. Not a custody reason: it never
/// refuses a launch, the hook is merely not run.
pub const REASON_CODEX_LIFECYCLE_HOOK_UNTRUSTED: &str = "codex_lifecycle_hook_untrusted";
/// Codex reason: the app-server handshake or `hooks/list` was refused or
/// malformed (a wire drift).
pub const REASON_CODEX_HANDSHAKE_FAILED: &str = "codex_handshake_failed";
/// Claude reason prefix: `claude_flag_unknown:<flag>`.
pub const REASON_CLAUDE_FLAG_UNKNOWN_PREFIX: &str = "claude_flag_unknown:";
/// Claude reason: the `initialize` control request was refused or malformed.
pub const REASON_CLAUDE_INITIALIZE_FAILED: &str = "claude_initialize_failed";
/// Claude reason: `initialize` lacks `claude_code_version` or `capabilities`.
pub const REASON_CLAUDE_CONTRACT_SHAPE: &str = "claude_contract_shape";
/// Claude reason: the host's OAuth login is not visible to RSI's non-`--bare`
/// launch shape, as if `--bare` had become the default for `-p`.
pub const REASON_CLAUDE_AMBIENT_AUTH_DROPPED: &str = "claude_ambient_auth_dropped";

/// The three Codex reasons that mean the custody hook will not run: the only
/// reasons a launch may be refused on.
pub const CODEX_CUSTODY_REASONS: [&str; 3] = [
    REASON_CODEX_HOOK_UNTRUSTED,
    REASON_CODEX_HOOK_HASH_MISMATCH,
    REASON_CODEX_HOOK_MISSING,
];

/// Whether `reason` is one of the custody reasons.
#[must_use]
pub fn is_custody_reason(reason: &str) -> bool {
    CODEX_CUSTODY_REASONS.contains(&reason)
}

/// The message of a Codex launch refused because the custody hook is not
/// trusted. It starts with the stable reason code.
#[must_use]
pub fn custody_refusal_message(reason: &str) -> String {
    format!(
        "{reason}: the installed codex-cli does not trust the sandbox custody hook RSI installs \
         (#2320), so a Codex session would run without branch custody; update RSI's hook trust \
         for this codex-cli, or set {SETTING_REFUSE_UNTRUSTED_CUSTODY_HOOK} to false to launch \
         without custody"
    )
}

/// Whether the CLI still honours the contract RSI relies on. `unchecked` means
/// no canary result exists (not run yet, disabled, or the CLI could not be
/// run); a launch is never refused on `unchecked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderContractState {
    Ok,
    Degraded,
    #[default]
    Unchecked,
}

impl ProviderContractState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Unchecked => "unchecked",
        }
    }
}

/// The canary result of one provider on the daemon's host.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderContractV1 {
    pub contract_state: ProviderContractState,
    /// Stable reason code when `degraded` or when an unchecked state has a
    /// cause (`cli_unavailable`, `canary_failed`, `canary_disabled`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The CLI's `--version` the canary ran against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    /// One bounded line naming the failing contract element of a `degraded`
    /// result (probe step, element, expected vs observed kind, CLI version;
    /// never raw CLI output). Absent for results that carry no detail and for
    /// older daemons, so an older client keeps parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `null` until the first canary finished.
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    /// The Codex model catalog's trust (#2761); only on the `codex` contract,
    /// absent until the daemon has parsed a catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<ProviderCatalogTrustV1>,
}

/// Whether the daemon trusts a Codex model catalog for capacity (#2761).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCatalogTrust {
    /// Every consumed field is present with its expected type.
    Trusted,
    /// A consumed field is missing or retyped: models are discoverable but feed
    /// no capacity.
    DiscoveryOnly,
}

impl ProviderCatalogTrust {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::DiscoveryOnly => "discovery_only",
        }
    }
}

/// The trust verdict of the parsed Codex model catalog and its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCatalogTrustV1 {
    pub trust: ProviderCatalogTrust,
    /// `structural_validation`, or `catalog_field_missing:<key>`,
    /// `catalog_field_type:<key>`, `catalog_models_empty` when discovery-only.
    pub reason: String,
    /// The `codex --version` the catalog was read from.
    pub cli_version: String,
    /// `sha256:` digest of the raw catalog bytes (evidence).
    pub content_digest: String,
    /// The version and digest match a reviewed contract (evidence only).
    pub reviewed_digest: bool,
    pub model_count: u32,
    /// Catalog keys no field contract names: recorded and tolerated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_fields: Vec<String>,
    pub observed_at: DateTime<Utc>,
}

impl ProviderContractV1 {
    /// Whether the result is a `degraded` custody failure.
    #[must_use]
    pub fn custody_failed(&self) -> bool {
        self.contract_state == ProviderContractState::Degraded
            && self.reason.as_deref().is_some_and(is_custody_reason)
    }
}

/// Most bytes of [`ProviderContractV1::detail`].
pub const MAX_CONTRACT_DETAIL_BYTES: usize = 256;

/// Bound `text` to one line of at most [`MAX_CONTRACT_DETAIL_BYTES`] bytes:
/// control characters become spaces and the cut falls on a char boundary.
#[must_use]
pub fn bound_contract_detail(text: &str) -> String {
    let mut line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if line.len() > MAX_CONTRACT_DETAIL_BYTES {
        let mut end = MAX_CONTRACT_DETAIL_BYTES;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}

/// Reason of an `unchecked` result: the CLI is not installed or not runnable.
pub const REASON_CLI_UNAVAILABLE: &str = "cli_unavailable";
/// Reason of an `unchecked` result: the canary itself could not run (timeout,
/// spawn failure); not evidence of drift.
pub const REASON_CANARY_FAILED: &str = "canary_failed";
/// Reason of an `unchecked` result: the operator turned the canary off.
pub const REASON_CANARY_DISABLED: &str = "canary_disabled";

/// The `AgentGetProviderStatus` entry names that carry a `contract`: the
/// Codex app-server runs the same CLI as `codex`.
#[must_use]
pub fn contract_provider_for_status_name(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some("claude"),
        "codex" | "codex_app_server" => Some("codex"),
        _ => None,
    }
}

/// One provider's row of `GetProviderContractState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderContractRowV1 {
    pub provider: String,
    pub contract: ProviderContractV1,
}

/// Result of `GetProviderContractState` and `RefreshProviderContractState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderContractStateResultV1 {
    pub generated_at: DateTime<Utc>,
    /// The `provider_contract_canary` setting in force.
    pub canary_enabled: bool,
    /// The `codex_refuse_untrusted_custody_hook` setting in force.
    pub codex_refuse_untrusted_custody_hook: bool,
    /// One row per entry of [`PROVIDER_CONTRACT_NAMES`] (or the one asked for).
    pub providers: Vec<ProviderContractRowV1>,
}

/// Strict request of both methods; omit `provider` for every provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderContractRequestV1 {
    #[serde(default)]
    pub provider: Option<String>,
}

impl ProviderContractRequestV1 {
    /// # Errors
    /// [`PROVIDER_CONTRACT_UNKNOWN_PROVIDER`] when `provider` is not checked.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self.provider.as_deref() {
            Some(name) if !PROVIDER_CONTRACT_NAMES.contains(&name) => {
                Err(PROVIDER_CONTRACT_UNKNOWN_PROVIDER)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_is_snake_case_and_omits_an_absent_reason() {
        let ok = ProviderContractV1 {
            contract_state: ProviderContractState::Ok,
            reason: None,
            cli_version: Some("codex-cli 0.163.0".into()),
            detail: None,
            checked_at: None,
            catalog: None,
        };
        let json = serde_json::to_value(&ok).unwrap();
        assert_eq!(json["contract_state"], "ok");
        assert!(json.get("reason").is_none());
        // An absent detail is omitted too, and a payload without it parses.
        assert!(json.get("detail").is_none());
        assert_eq!(json["cli_version"], "codex-cli 0.163.0");
        let back: ProviderContractV1 = serde_json::from_value(json).unwrap();
        assert_eq!(back, ok);
        assert_eq!(
            serde_json::to_value(ProviderContractV1::default()).unwrap()["contract_state"],
            "unchecked"
        );
        // An absent catalog is omitted, so an older client never sees the key.
        assert!(
            serde_json::to_value(ProviderContractV1::default())
                .unwrap()
                .get("catalog")
                .is_none()
        );
    }

    #[test]
    fn detail_is_additive_and_bounded_to_one_line() {
        let degraded = ProviderContractV1 {
            contract_state: ProviderContractState::Degraded,
            reason: Some(REASON_CLAUDE_CONTRACT_SHAPE.into()),
            detail: Some("probe=initialize element=capabilities".into()),
            ..ProviderContractV1::default()
        };
        let json = serde_json::to_value(&degraded).unwrap();
        assert_eq!(json["detail"], "probe=initialize element=capabilities");
        let back: ProviderContractV1 = serde_json::from_value(json).unwrap();
        assert_eq!(back, degraded);
        // An older daemon's payload has no `detail`.
        let old: ProviderContractV1 =
            serde_json::from_str(r#"{"contract_state":"degraded","reason":"x"}"#).unwrap();
        assert_eq!(old.detail, None);

        let long = format!("a\nb{}", "é".repeat(400));
        let bounded = bound_contract_detail(&long);
        assert!(
            bounded.len() <= MAX_CONTRACT_DETAIL_BYTES,
            "{}",
            bounded.len()
        );
        assert!(!bounded.contains('\n'));
        assert!(bounded.starts_with("a b"));
    }

    #[test]
    fn catalog_trust_round_trips_with_snake_case_trust_and_reason() {
        let contract = ProviderContractV1 {
            catalog: Some(ProviderCatalogTrustV1 {
                trust: ProviderCatalogTrust::DiscoveryOnly,
                reason: "catalog_field_missing:context_window".into(),
                cli_version: "codex-cli 0.163.0".into(),
                content_digest: "sha256:abc".into(),
                reviewed_digest: false,
                model_count: 11,
                unknown_fields: Vec::new(),
                observed_at: Utc::now(),
            }),
            ..ProviderContractV1::default()
        };
        let json = serde_json::to_value(&contract).unwrap();
        assert_eq!(json["catalog"]["trust"], "discovery_only");
        assert_eq!(
            json["catalog"]["reason"],
            "catalog_field_missing:context_window"
        );
        assert!(json["catalog"].get("unknown_fields").is_none());
        let back: ProviderContractV1 = serde_json::from_value(json).unwrap();
        assert_eq!(back, contract);
    }

    #[test]
    fn only_the_three_custody_reasons_are_custody_failures() {
        let degraded = |reason: &str| ProviderContractV1 {
            contract_state: ProviderContractState::Degraded,
            reason: Some(reason.into()),
            ..ProviderContractV1::default()
        };
        for reason in [
            REASON_CODEX_HOOK_UNTRUSTED,
            REASON_CODEX_HOOK_HASH_MISMATCH,
            REASON_CODEX_HOOK_MISSING,
        ] {
            assert!(degraded(reason).custody_failed(), "{reason}");
        }
        for reason in [
            REASON_CODEX_HANDSHAKE_FAILED,
            REASON_CLAUDE_INITIALIZE_FAILED,
            "claude_flag_unknown:--max-turns",
        ] {
            assert!(!degraded(reason).custody_failed(), "{reason}");
        }
        // An unchecked result with a custody-looking reason never refuses.
        assert!(
            !ProviderContractV1 {
                contract_state: ProviderContractState::Unchecked,
                reason: Some(REASON_CODEX_HOOK_UNTRUSTED.into()),
                ..ProviderContractV1::default()
            }
            .custody_failed()
        );
    }

    #[test]
    fn the_refusal_message_starts_with_the_code_and_names_the_setting() {
        let message = custody_refusal_message(REASON_CODEX_HOOK_UNTRUSTED);
        assert!(message.starts_with("codex_hook_untrusted:"), "{message}");
        assert!(
            message.contains(SETTING_REFUSE_UNTRUSTED_CUSTODY_HOOK),
            "{message}"
        );
    }

    #[test]
    fn status_names_map_to_the_checked_provider() {
        assert_eq!(contract_provider_for_status_name("claude"), Some("claude"));
        assert_eq!(contract_provider_for_status_name("codex"), Some("codex"));
        assert_eq!(
            contract_provider_for_status_name("codex_app_server"),
            Some("codex")
        );
        assert_eq!(contract_provider_for_status_name("openrouter"), None);
    }

    #[test]
    fn request_rejects_unknown_fields_and_names() {
        assert!(
            serde_json::from_value::<ProviderContractRequestV1>(serde_json::json!({"caller": "x"}))
                .is_err()
        );
        let bad = ProviderContractRequestV1 {
            provider: Some("openrouter".into()),
        };
        assert_eq!(bad.validate(), Err(PROVIDER_CONTRACT_UNKNOWN_PROVIDER));
        assert_eq!(ProviderContractRequestV1::default().validate(), Ok(()));
    }
}
