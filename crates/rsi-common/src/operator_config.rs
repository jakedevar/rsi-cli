//! Operator-only configuration RPCs the TUI and the desktop share (hub #3039).
//!
//! Three families the TUI used to edit as local files now go through the
//! daemon, so every frontend reads and writes one source of truth:
//!
//! - Claude Code hooks: the `hooks` block of `~/.claude/settings.json`,
//!   compare-and-swap on a file revision (the TUI's save-conflict check).
//! - Custom OpenAI-compatible providers: nonsecret rows in the daemon store;
//!   the API key goes to the key vault and is never returned (reads carry
//!   `api_key.state` and a fingerprint hint only).
//! - Message bridges: `signal.toml` / `imessage.toml` in the RSI data dir.
//!
//! Every method is operator-only: absent from `AGENT_VERBS`, `READ_VERBS`,
//! native tools, the agent CLI catalog and rsi-remote (AGENTS.md rule 10).
//! Request types that carry a secret have a redacted `Debug`; no response
//! type has a field that can hold one.

use crate::claude_hooks::ClaudeHooks;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub const METHOD_LIST_CLAUDE_HOOKS: &str = "ListClaudeHooks";
pub const METHOD_PUT_CLAUDE_HOOKS: &str = "PutClaudeHooks";
pub const METHOD_DELETE_CLAUDE_HOOKS: &str = "DeleteClaudeHooks";
pub const METHOD_LIST_CUSTOM_PROVIDERS: &str = "ListCustomProviders";
pub const METHOD_GET_CUSTOM_PROVIDER: &str = "GetCustomProvider";
pub const METHOD_PUT_CUSTOM_PROVIDER: &str = "PutCustomProvider";
pub const METHOD_DELETE_CUSTOM_PROVIDER: &str = "DeleteCustomProvider";
pub const METHOD_SET_CUSTOM_PROVIDER_ROUTE: &str = "SetCustomProviderRoute";
pub const METHOD_LIST_CUSTOM_PROVIDER_ROUTES: &str = "ListCustomProviderRoutes";
pub const METHOD_LIST_MESSAGE_BRIDGES: &str = "ListMessageBridges";
pub const METHOD_GET_MESSAGE_BRIDGE: &str = "GetMessageBridge";
pub const METHOD_PUT_MESSAGE_BRIDGE: &str = "PutMessageBridge";
pub const METHOD_DELETE_MESSAGE_BRIDGE: &str = "DeleteMessageBridge";

/// Every method of this family. The operator-only fence test iterates it.
pub const OPERATOR_METHODS: [&str; 13] = [
    METHOD_LIST_CLAUDE_HOOKS,
    METHOD_PUT_CLAUDE_HOOKS,
    METHOD_DELETE_CLAUDE_HOOKS,
    METHOD_LIST_CUSTOM_PROVIDERS,
    METHOD_GET_CUSTOM_PROVIDER,
    METHOD_PUT_CUSTOM_PROVIDER,
    METHOD_DELETE_CUSTOM_PROVIDER,
    METHOD_SET_CUSTOM_PROVIDER_ROUTE,
    METHOD_LIST_CUSTOM_PROVIDER_ROUTES,
    METHOD_LIST_MESSAGE_BRIDGES,
    METHOD_GET_MESSAGE_BRIDGE,
    METHOD_PUT_MESSAGE_BRIDGE,
    METHOD_DELETE_MESSAGE_BRIDGE,
];

/// Refusal prefix for a session-attributed (tokened) caller.
pub const OPERATOR_ONLY_REFUSAL: &str = "operator_config_operator_only";
/// Typed error code of a `PutClaudeHooks` whose `expected_revision` is stale.
pub const CLAUDE_HOOKS_CONFLICT: &str = "claude_hooks_conflict";
/// The revision `ListClaudeHooks` reports while the settings file is absent.
pub const CLAUDE_HOOKS_ABSENT_REVISION: &str = "absent";

pub const MAX_CUSTOM_PROVIDERS: usize = 64;
pub const MAX_CUSTOM_PROVIDER_NAME_BYTES: usize = 128;
pub const MAX_CUSTOM_PROVIDER_URL_BYTES: usize = 2048;
pub const MAX_CUSTOM_PROVIDER_MODEL_BYTES: usize = 256;
pub const MAX_BRIDGE_FIELD_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// Claude Code hooks
// ---------------------------------------------------------------------------

/// `ListClaudeHooks` result and `PutClaudeHooks` result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeHooksView {
    /// Absolute path of the settings file on the daemon host.
    pub path: String,
    pub exists: bool,
    /// Opaque revision of the whole file (bytes and modification time), or
    /// [`CLAUDE_HOOKS_ABSENT_REVISION`]. Pass it back as `expected_revision`.
    pub revision: String,
    pub hooks: ClaudeHooks,
}

/// `PutClaudeHooks`: replace the whole `hooks` block. Every other top-level
/// key of the file is preserved. Refused with [`CLAUDE_HOOKS_CONFLICT`]
/// (error `data.current_revision`) when the file changed since
/// `expected_revision` was read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutClaudeHooksParams {
    pub expected_revision: String,
    pub hooks: ClaudeHooks,
}

/// `DeleteClaudeHooks`: remove the `hooks` key from the settings file (every
/// other top-level key is preserved). Same compare-and-swap on
/// `expected_revision` as [`PutClaudeHooksParams`]; an absent file stays
/// absent. The result is the [`ClaudeHooksView`] afterwards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteClaudeHooksParams {
    pub expected_revision: String,
}

// ---------------------------------------------------------------------------
// Custom providers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomProviderKeyState {
    /// A key is held in the vault.
    Set,
    /// No key is stored (a keyless local server, or never set).
    Absent,
}

/// Secret-free key metadata. `fingerprint` is the vault's SHA-256 prefix
/// hint (`rsid-store` `vault/secret.rs`), never a part of the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProviderKeyMetadata {
    pub state: CustomProviderKeyState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at: Option<DateTime<Utc>>,
}

/// One custom provider as every read returns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProviderSummary {
    pub id: Uuid,
    pub name: String,
    pub base_url: String,
    pub default_model: String,
    pub api_key: CustomProviderKeyMetadata,
}

/// `ListCustomProviders` result, in the operator's order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListCustomProvidersResult {
    pub providers: Vec<CustomProviderSummary>,
}

/// `GetCustomProvider` / `DeleteCustomProvider` params.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProviderIdParams {
    pub id: Uuid,
}

/// What `PutCustomProvider` does with the key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CustomProviderKeyChange {
    /// Leave the stored key as it is (the default; also for a new provider,
    /// which then has none).
    #[default]
    Keep,
    /// Store this key in the vault, replacing any previous one.
    Set { secret: String },
    /// Remove the stored key.
    Clear,
}

impl fmt::Debug for CustomProviderKeyChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("Keep"),
            Self::Set { .. } => formatter.write_str("Set { secret: <redacted> }"),
            Self::Clear => formatter.write_str("Clear"),
        }
    }
}

/// `PutCustomProvider`: create (`id` absent) or replace one provider.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutCustomProviderParams {
    #[serde(default)]
    pub id: Option<Uuid>,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub api_key: CustomProviderKeyChange,
}

impl fmt::Debug for PutCustomProviderParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PutCustomProviderParams")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("default_model", &self.default_model)
            .field("api_key", &self.api_key)
            .finish()
    }
}

/// `DeleteCustomProvider` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteCustomProviderResult {
    pub id: Uuid,
    pub deleted: bool,
    /// Model roles that pointed at the provider and were cleared.
    #[serde(default)]
    pub cleared_routes: Vec<CustomProviderRole>,
}

/// A daemon model role that can run against a custom provider. The daemon
/// resolves the provider's base URL and vault key itself, so no frontend
/// ever holds the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomProviderRole {
    TitleModel,
    MemoryModelFallback,
    DreamModel,
    PromptCompileModel,
}

impl CustomProviderRole {
    pub const ALL: [Self; 4] = [
        Self::TitleModel,
        Self::MemoryModelFallback,
        Self::DreamModel,
        Self::PromptCompileModel,
    ];

    /// The `RuntimeConfig` field prefix (`<prefix>_base_url`, `<prefix>_api_key`).
    #[must_use]
    pub const fn config_prefix(self) -> &'static str {
        match self {
            Self::TitleModel => "title_model",
            Self::MemoryModelFallback => "memory_model_fallback",
            Self::DreamModel => "dream_model",
            Self::PromptCompileModel => "prompt_compile_model",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TitleModel => "title_model",
            Self::MemoryModelFallback => "memory_model_fallback",
            Self::DreamModel => "dream_model",
            Self::PromptCompileModel => "prompt_compile_model",
        }
    }
}

/// `SetCustomProviderRoute`: point a model role at a custom provider
/// (`id`), or detach it (`id: null`, which clears the role's base URL and
/// key). The assignment is stored and reapplied at daemon start and on every
/// change to that provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCustomProviderRouteParams {
    pub role: CustomProviderRole,
    #[serde(default)]
    pub id: Option<Uuid>,
}

/// `SetCustomProviderRoute` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProviderRouteResult {
    pub role: CustomProviderRole,
    #[serde(default)]
    pub id: Option<Uuid>,
    /// The provider has a vault key the role now uses.
    pub api_key_set: bool,
}

/// One model role's stored route, as `ListCustomProviderRoutes` returns it.
/// The provider's key never appears: only the vault's state and fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProviderRouteView {
    pub role: CustomProviderRole,
    /// The routed provider, or `null` while the role is detached (also when
    /// the stored provider no longer exists).
    #[serde(default)]
    pub id: Option<Uuid>,
    /// The routed provider's name; `null` while detached.
    #[serde(default)]
    pub name: Option<String>,
    /// The routed provider's key metadata; `null` while detached.
    #[serde(default)]
    pub api_key: Option<CustomProviderKeyMetadata>,
}

/// `ListCustomProviderRoutes` result: one entry per role in
/// `CustomProviderRole::ALL` order, whether routed or not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListCustomProviderRoutesResult {
    pub routes: Vec<CustomProviderRouteView>,
}

// ---------------------------------------------------------------------------
// Message bridges
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageBridgeKindV1 {
    Signal,
    Imessage,
}

impl MessageBridgeKindV1 {
    pub const ALL: [Self; 2] = [Self::Signal, Self::Imessage];

    #[must_use]
    pub const fn config_filename(self) -> &'static str {
        match self {
            Self::Signal => "signal.toml",
            Self::Imessage => "imessage.toml",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Signal => "Signal",
            Self::Imessage => "iMessage",
        }
    }
}

/// The connection settings a bridge config carries. `allow_from` is the
/// comma-separated allowlist the form edits; `account` is ignored by iMessage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageBridgeConfigV1 {
    pub enabled: bool,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub allow_from: String,
    pub working_dir: String,
}

/// One bridge as `ListMessageBridges` / `GetMessageBridge` / `PutMessageBridge`
/// return it. `config` is `None` while the file is absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageBridgeView {
    pub kind: MessageBridgeKindV1,
    pub path: String,
    #[serde(default)]
    pub config: Option<MessageBridgeConfigV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListMessageBridgesResult {
    pub bridges: Vec<MessageBridgeView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetMessageBridgeParams {
    pub kind: MessageBridgeKindV1,
}

/// `PutMessageBridge`: write the bridge config (atomically). The bridge
/// process reads it at its next start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutMessageBridgeParams {
    pub kind: MessageBridgeKindV1,
    pub config: MessageBridgeConfigV1,
}

/// `DeleteMessageBridge`: remove the bridge's config file. Idempotent; the
/// result is the [`MessageBridgeView`] afterwards (`config: null`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteMessageBridgeParams {
    pub kind: MessageBridgeKindV1,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_bearing_requests_redact_debug() {
        let params = PutCustomProviderParams {
            id: None,
            name: "Groq".into(),
            base_url: "https://example.invalid/v1".into(),
            default_model: String::new(),
            api_key: CustomProviderKeyChange::Set {
                secret: "fake-key-operator-config-3039".into(),
            },
        };
        let debug = format!("{params:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("Groq"), "{debug}");
        assert_eq!(
            debug.matches("fake-key-operator-config-3039").count(),
            0,
            "{debug}"
        );
    }

    #[test]
    fn key_change_defaults_to_keep_and_parses_each_action() {
        let params: PutCustomProviderParams =
            serde_json::from_value(serde_json::json!({"name": "a", "base_url": "http://x"}))
                .unwrap();
        assert_eq!(params.api_key, CustomProviderKeyChange::Keep);
        let set: CustomProviderKeyChange =
            serde_json::from_value(serde_json::json!({"action": "set", "secret": "s"})).unwrap();
        assert_eq!(set, CustomProviderKeyChange::Set { secret: "s".into() });
        let clear: CustomProviderKeyChange =
            serde_json::from_value(serde_json::json!({"action": "clear"})).unwrap();
        assert_eq!(clear, CustomProviderKeyChange::Clear);
    }

    #[test]
    fn role_prefixes_name_runtime_config_fields() {
        let prefixes: Vec<_> = CustomProviderRole::ALL
            .iter()
            .map(|role| role.config_prefix())
            .collect();
        assert_eq!(
            prefixes,
            [
                "title_model",
                "memory_model_fallback",
                "dream_model",
                "prompt_compile_model"
            ]
        );
    }
}
