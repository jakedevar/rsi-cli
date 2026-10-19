//! `ListMessageBridges` / `GetMessageBridge` / `PutMessageBridge` /
//! `DeleteMessageBridge`: the Signal
//! and iMessage bridge configs in the RSI data dir (hub #3039, formerly the
//! TUI's own `signal.toml` / `imessage.toml` write).
//!
//! The parse and render are the TUI's (`crates/rsi/src/settings.rs` before
//! this change), so a file either side wrote reads the same. The write is now
//! atomic and keeps an existing file's permissions.

use super::OperatorConfigPaths;
use crate::error::{DaemonError, Result};
use rsi_common::operator_config::{
    DeleteMessageBridgeParams, GetMessageBridgeParams, ListMessageBridgesResult,
    MAX_BRIDGE_FIELD_BYTES, MessageBridgeConfigV1, MessageBridgeKindV1, MessageBridgeView,
    PutMessageBridgeParams,
};
use std::path::Path;

/// `ListMessageBridges`.
///
/// # Errors
///
/// `Store` when a present file cannot be read or parsed.
pub fn list(paths: &OperatorConfigPaths) -> Result<ListMessageBridgesResult> {
    let bridges = MessageBridgeKindV1::ALL
        .into_iter()
        .map(|kind| view(paths, kind))
        .collect::<Result<_>>()?;
    Ok(ListMessageBridgesResult { bridges })
}

/// `GetMessageBridge`.
///
/// # Errors
///
/// `Store` when the file cannot be read or parsed.
pub fn get(
    paths: &OperatorConfigPaths,
    request: GetMessageBridgeParams,
) -> Result<MessageBridgeView> {
    view(paths, request.kind)
}

/// `PutMessageBridge`.
///
/// # Errors
///
/// `InvalidParam` for an invalid config, `Store` for a write failure.
pub fn put(
    paths: &OperatorConfigPaths,
    request: PutMessageBridgeParams,
) -> Result<MessageBridgeView> {
    let config = validate(request.kind, request.config)?;
    let path = paths.bridge(request.kind);
    super::write_atomic(path, render(request.kind, &config).as_bytes()).map_err(|error| {
        DaemonError::Store(format!(
            "could not write {}: {:?}",
            path.display(),
            error.kind()
        ))
    })?;
    view(paths, request.kind)
}

/// `DeleteMessageBridge`: remove the bridge's config file. A file that is
/// already absent is not an error (the call is idempotent); the result is the
/// bridge as it reads afterwards (`config: null`).
///
/// # Errors
///
/// `Store` when the file exists and cannot be removed.
pub fn delete(
    paths: &OperatorConfigPaths,
    request: DeleteMessageBridgeParams,
) -> Result<MessageBridgeView> {
    let path = paths.bridge(request.kind);
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DaemonError::Store(format!(
                "could not remove {}: {:?}",
                path.display(),
                error.kind()
            )));
        }
    }
    view(paths, request.kind)
}

fn view(paths: &OperatorConfigPaths, kind: MessageBridgeKindV1) -> Result<MessageBridgeView> {
    let path = paths.bridge(kind);
    Ok(MessageBridgeView {
        kind,
        path: path.display().to_string(),
        config: load(path)?,
    })
}

fn field(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.len() > MAX_BRIDGE_FIELD_BYTES {
        return Err(DaemonError::InvalidParam(format!(
            "{name} is longer than {MAX_BRIDGE_FIELD_BYTES} bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(DaemonError::InvalidParam(format!(
            "{name} must be a single line without control characters"
        )));
    }
    Ok(value.to_owned())
}

fn validate(
    kind: MessageBridgeKindV1,
    config: MessageBridgeConfigV1,
) -> Result<MessageBridgeConfigV1> {
    let config = MessageBridgeConfigV1 {
        enabled: config.enabled,
        account: field("account", &config.account)?,
        allow_from: field("allow_from", &config.allow_from)?,
        working_dir: field("working_dir", &config.working_dir)?,
    };
    if kind == MessageBridgeKindV1::Signal && config.account.is_empty() {
        return Err(DaemonError::InvalidParam("Signal account required".into()));
    }
    if config.working_dir.is_empty() {
        return Err(DaemonError::InvalidParam(
            "Working directory required".into(),
        ));
    }
    Ok(config)
}

fn load(path: &Path) -> Result<Option<MessageBridgeConfigV1>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(DaemonError::Store(format!(
                "could not read {}: {:?}",
                path.display(),
                error.kind()
            )));
        }
    };
    let value: toml::Value = toml::from_str(&contents)
        .map_err(|_| DaemonError::Store(format!("{} is not valid TOML", path.display())))?;
    let table = value
        .as_table()
        .ok_or_else(|| DaemonError::Store(format!("{} is not a TOML table", path.display())))?;
    let mut config = MessageBridgeConfigV1 {
        enabled: true,
        account: String::new(),
        allow_from: String::new(),
        working_dir: String::new(),
    };
    if let Some(enabled) = table.get("enabled").and_then(toml::Value::as_bool) {
        config.enabled = enabled;
    }
    if let Some(account) = table.get("account").and_then(scalar_to_string) {
        config.account = account;
    }
    if let Some(handles) = table
        .get("allow_from")
        .or_else(|| table.get("allowlist"))
        .and_then(string_array)
    {
        config.allow_from = handles.join(", ");
    }
    if let Some(working_dir) = table
        .get("default_working_dir")
        .or_else(|| table.get("working_dir"))
        .and_then(scalar_to_string)
    {
        config.working_dir = working_dir;
    }
    Ok(Some(config))
}

fn render(kind: MessageBridgeKindV1, config: &MessageBridgeConfigV1) -> String {
    let handles = split_handles(&config.allow_from);
    let dm_policy = if handles.is_empty() {
        "open"
    } else {
        "allowlist"
    };
    let allow_from = toml_string_array(&handles);
    let working_dir = toml_escape(&config.working_dir);
    match kind {
        MessageBridgeKindV1::Signal => format!(
            "enabled = {}\n\
poll_interval_ms = 2000\n\
dm_policy = \"{}\"\n\
allow_from = {}\n\
max_message_length = 4000\n\
echo_cache_ttl_ms = 5000\n\
debounce_ms = 500\n\
default_working_dir = \"{}\"\n\
default_provider = \"Claude\"\n\
account = \"{}\"\n",
            config.enabled,
            dm_policy,
            allow_from,
            working_dir,
            toml_escape(&config.account)
        ),
        MessageBridgeKindV1::Imessage => format!(
            "enabled = {}\n\
poll_interval_ms = 2000\n\
dm_policy = \"{}\"\n\
allow_from = {}\n\
max_message_length = 4000\n\
echo_cache_ttl_ms = 5000\n\
debounce_ms = 500\n\
default_working_dir = \"{}\"\n\
default_provider = \"Claude\"\n",
            config.enabled, dm_policy, allow_from, working_dir
        ),
    }
}

fn split_handles(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|handle| !handle.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn toml_string_array(items: &[String]) -> String {
    let body = items
        .iter()
        .map(|item| format!("\"{}\"", toml_escape(item)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{body}]")
}

fn toml_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn scalar_to_string(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(text) => Some(text.clone()),
        toml::Value::Integer(number) => Some(number.to_string()),
        _ => None,
    }
}

fn string_array(value: &toml::Value) -> Option<Vec<String>> {
    value.as_array()?.iter().map(scalar_to_string).collect()
}
