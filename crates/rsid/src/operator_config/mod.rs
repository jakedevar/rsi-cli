//! Operator-only configuration RPCs shared by the TUI and the desktop
//! (hub #3039, wire types in [`rsi_common::operator_config`]).
//!
//! - [`hooks`]: the `hooks` block of `~/.claude/settings.json`, written
//!   atomically with a compare-and-swap on the file revision.
//! - [`custom_providers`]: nonsecret rows under `custom_provider.<id>` in
//!   `daemon_settings`; API keys in the key vault's own custom-provider file,
//!   never returned. Model roles (`SetCustomProviderRoute`) resolve the key
//!   inside the daemon. A one-time import copies the TUI's legacy
//!   `state.json` providers in without touching that file.
//! - [`bridges`]: `signal.toml` / `imessage.toml` in the RSI data dir.
//!
//! Every method is operator-only: absent from `AGENT_VERBS`, `READ_VERBS`,
//! native tools, the agent CLI catalog and rsi-remote. A tokened caller is
//! refused before its params are parsed, so a malformed request cannot echo a
//! secret.

pub mod bridges;
pub mod custom_providers;
pub mod hooks;
pub mod legacy_import;

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::vault::VaultHandle;
use rsi_common::operator_config::{
    METHOD_DELETE_CLAUDE_HOOKS, METHOD_DELETE_CUSTOM_PROVIDER, METHOD_DELETE_MESSAGE_BRIDGE,
    METHOD_GET_CUSTOM_PROVIDER, METHOD_GET_MESSAGE_BRIDGE, METHOD_LIST_CLAUDE_HOOKS,
    METHOD_LIST_CUSTOM_PROVIDER_ROUTES, METHOD_LIST_CUSTOM_PROVIDERS, METHOD_LIST_MESSAGE_BRIDGES,
    METHOD_PUT_CLAUDE_HOOKS, METHOD_PUT_CUSTOM_PROVIDER, METHOD_PUT_MESSAGE_BRIDGE,
    METHOD_SET_CUSTOM_PROVIDER_ROUTE, MessageBridgeKindV1, OPERATOR_METHODS, OPERATOR_ONLY_REFUSAL,
};
use serde::Deserialize;
use std::path::PathBuf;

#[must_use]
pub fn is_operator_method(method: &str) -> bool {
    OPERATOR_METHODS.contains(&method)
}

/// The host files this family edits. Production paths come from the
/// existing helpers (`dirs::home_dir`, `rsi_common::identity::data_path`);
/// tests point them at a temporary directory.
#[derive(Clone, Debug)]
pub struct OperatorConfigPaths {
    pub claude_settings: PathBuf,
    pub signal_bridge: PathBuf,
    pub imessage_bridge: PathBuf,
    /// The TUI's persisted state (`state.json`) the one-time import reads.
    pub legacy_tui_state: PathBuf,
}

impl OperatorConfigPaths {
    #[must_use]
    pub fn production() -> Self {
        let bridge = |kind: MessageBridgeKindV1| {
            rsi_common::identity::data_path(kind.config_filename(), kind.config_filename())
        };
        Self {
            claude_settings: dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("settings.json"),
            signal_bridge: bridge(MessageBridgeKindV1::Signal),
            imessage_bridge: bridge(MessageBridgeKindV1::Imessage),
            legacy_tui_state: rsi_common::identity::data_path("state.json", "state.json"),
        }
    }

    /// Production paths, except under unit tests, where every server built
    /// without explicit paths gets a fresh temporary root so no test can read
    /// or write the operator's own `settings.json` or `state.json`.
    #[must_use]
    pub fn default_for_build() -> Self {
        #[cfg(test)]
        {
            Self::under(&std::env::temp_dir().join(format!(
                "rsid-operator-config-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            )))
        }
        #[cfg(not(test))]
        {
            Self::production()
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn under(root: &std::path::Path) -> Self {
        Self {
            claude_settings: root.join("home/.claude/settings.json"),
            signal_bridge: root.join("data/signal.toml"),
            imessage_bridge: root.join("data/imessage.toml"),
            legacy_tui_state: root.join("data/state.json"),
        }
    }

    #[must_use]
    pub fn bridge(&self, kind: MessageBridgeKindV1) -> &std::path::Path {
        match kind {
            MessageBridgeKindV1::Signal => &self.signal_bridge,
            MessageBridgeKindV1::Imessage => &self.imessage_bridge,
        }
    }
}

/// What one call needs. The store guard is the caller's.
pub struct OperatorConfigContext<'a> {
    pub store: &'a Store,
    pub vault: &'a VaultHandle,
    pub runtime_config: &'a RuntimeConfig,
    pub paths: &'a OperatorConfigPaths,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

/// Parse a request. The error never echoes the params, so a malformed
/// request carrying a key cannot put it into an error, log or event.
pub(crate) fn parse<T: for<'de> Deserialize<'de>>(
    method: &str,
    params: &serde_json::Value,
) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|_| {
        DaemonError::InvalidParam(format!(
            "invalid {method} params; see rsi_common::operator_config for the request shape"
        ))
    })
}

fn parse_empty(method: &str, params: &serde_json::Value) -> Result<()> {
    if params.is_null() {
        return Ok(());
    }
    parse::<NoParams>(method, params).map(|_| ())
}

pub(crate) fn to_value<T: serde::Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(DaemonError::from)
}

/// Dispatch one operator-only configuration RPC.
///
/// # Errors
///
/// `PolicyDenied` for a tokened caller, `InvalidParam` for an invalid or
/// unknown request, the typed `claude_hooks_conflict` for a stale hooks
/// revision, and `Store` for persistence failures (never carrying a secret).
pub fn handle(
    context: &OperatorConfigContext<'_>,
    method: &str,
    session_token_present: bool,
    params: &serde_json::Value,
) -> Result<serde_json::Value> {
    if session_token_present {
        return Err(DaemonError::PolicyDenied(format!(
            "{OPERATOR_ONLY_REFUSAL}: `{method}` is operator-only and is not available to session-attributed callers"
        )));
    }
    match method {
        METHOD_LIST_CLAUDE_HOOKS => {
            parse_empty(method, params)?;
            to_value(&hooks::list(&context.paths.claude_settings)?)
        }
        METHOD_PUT_CLAUDE_HOOKS => {
            let request = parse(method, params)?;
            to_value(&hooks::put(&context.paths.claude_settings, request)?)
        }
        METHOD_DELETE_CLAUDE_HOOKS => {
            let request = parse(method, params)?;
            to_value(&hooks::delete(&context.paths.claude_settings, request)?)
        }
        METHOD_LIST_CUSTOM_PROVIDERS => {
            parse_empty(method, params)?;
            to_value(&custom_providers::list(context.store, context.vault)?)
        }
        METHOD_GET_CUSTOM_PROVIDER => {
            let request = parse(method, params)?;
            to_value(&custom_providers::get(context, request)?)
        }
        METHOD_PUT_CUSTOM_PROVIDER => {
            let request = parse(method, params)?;
            to_value(&custom_providers::put(context, request)?)
        }
        METHOD_DELETE_CUSTOM_PROVIDER => {
            let request = parse(method, params)?;
            to_value(&custom_providers::delete(context, request)?)
        }
        METHOD_LIST_CUSTOM_PROVIDER_ROUTES => {
            parse_empty(method, params)?;
            to_value(&custom_providers::list_routes(
                context.store,
                context.vault,
            )?)
        }
        METHOD_SET_CUSTOM_PROVIDER_ROUTE => {
            let request = parse(method, params)?;
            to_value(&custom_providers::set_route(context, request)?)
        }
        METHOD_LIST_MESSAGE_BRIDGES => {
            parse_empty(method, params)?;
            to_value(&bridges::list(context.paths)?)
        }
        METHOD_GET_MESSAGE_BRIDGE => {
            let request = parse(method, params)?;
            to_value(&bridges::get(context.paths, request)?)
        }
        METHOD_PUT_MESSAGE_BRIDGE => {
            let request = parse(method, params)?;
            to_value(&bridges::put(context.paths, request)?)
        }
        METHOD_DELETE_MESSAGE_BRIDGE => {
            let request = parse(method, params)?;
            to_value(&bridges::delete(context.paths, request)?)
        }
        other => Err(DaemonError::InvalidParam(format!(
            "unknown operator configuration method `{other}`"
        ))),
    }
}

/// Write `bytes` to `path` atomically: a temporary file in the same
/// directory, fsync, then rename. An existing file's permissions carry over.
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let temp = parent.join(format!(
        ".{name}.rsid-tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, metadata.permissions())?;
        }
        std::fs::rename(&temp, path)
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod legacy_import_tests;
#[cfg(test)]
mod tests;
