//! Operator-owned MCP server configuration and secret custody (#788).
//!
//! Nonsecret definitions are one JSON row per canonical id under
//! `mcp.server.<id>`. Credential bytes live only in the key-vault namespace;
//! settings rows never receive them. The six RPC methods here are
//! operator-only and are absent from every agent catalog.

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::vault::{VaultError, VaultHandle};
use rsi_common::mcp::{
    ListMcpServersResult, MAX_MCP_SERVERS, METHOD_CLEAR_SECRET, METHOD_LIST, METHOD_ROTATE_SECRET,
    METHOD_SET_ENABLED, METHOD_SET_SECRET, METHOD_UPSERT, McpServerDefinition, McpServerIdParams,
    McpServerSummary, OPERATOR_METHODS, SetMcpServerEnabledParams, UpsertMcpServerParams,
    validate_mcp_server_definition, validate_mcp_server_id,
};
use serde::Deserialize;
use std::collections::BTreeMap;

pub const MCP_SERVER_SETTING_PREFIX: &str = "mcp.server.";
pub const OPERATOR_ONLY_REFUSAL: &str = "mcp_config_operator_only";

#[must_use]
pub fn is_operator_method(method: &str) -> bool {
    OPERATOR_METHODS.contains(&method)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

fn parse<T: for<'de> Deserialize<'de>>(method: &str, params: &serde_json::Value) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|_| {
        DaemonError::InvalidParam(format!(
            "invalid {method} params; see rsi_common::mcp for the request shape"
        ))
    })
}

fn parse_empty(method: &str, params: &serde_json::Value) -> Result<()> {
    if params.is_null() {
        return Ok(());
    }
    parse::<NoParams>(method, params).map(|_| ())
}

fn parse_secret(
    method: &str,
    params: &serde_json::Value,
) -> Result<(String, zeroize::Zeroizing<String>)> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_secret_params(method))?;
    let id = object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_secret_params(method))?;
    let secret = object
        .get("secret")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_secret_params(method))?;
    if object.len() != 2 {
        return Err(invalid_secret_params(method));
    }
    Ok((id.to_owned(), zeroize::Zeroizing::new(secret.to_owned())))
}

fn invalid_secret_params(method: &str) -> DaemonError {
    DaemonError::InvalidParam(format!(
        "invalid {method} params; see rsi_common::mcp for the request shape"
    ))
}

fn setting_key(id: &str) -> String {
    format!("{MCP_SERVER_SETTING_PREFIX}{id}")
}

fn parse_definition(id: &str, raw: &str) -> Result<McpServerDefinition> {
    let definition: McpServerDefinition = serde_json::from_str(raw).map_err(|_| {
        DaemonError::Store(format!(
            "stored mcp server configuration for {id} is malformed"
        ))
    })?;
    validate_mcp_server_definition(&definition)
        .map_err(|_| DaemonError::Store("stored mcp server configuration is invalid".into()))?;
    if definition.id != id {
        return Err(DaemonError::Store(
            "stored mcp server configuration has a mismatched id".into(),
        ));
    }
    Ok(definition)
}

fn load_definitions(store: &Store) -> Result<BTreeMap<String, McpServerDefinition>> {
    let mut definitions = BTreeMap::new();
    for (key, raw) in store.list_daemon_settings_with_prefix(MCP_SERVER_SETTING_PREFIX)? {
        let Some(id) = key.strip_prefix(MCP_SERVER_SETTING_PREFIX) else {
            continue;
        };
        definitions.insert(id.to_owned(), parse_definition(id, &raw)?);
    }
    Ok(definitions)
}

/// Load only enabled definitions. A malformed or mismatched row is reported
/// by id so one bad operator row cannot prevent every other server from
/// loading; the session bridge turns each id into a nonsecret unavailable event.
pub(crate) fn load_enabled_definitions_lossy(
    store: &Store,
) -> (BTreeMap<String, McpServerDefinition>, Vec<String>) {
    let mut definitions = BTreeMap::new();
    let mut unavailable = Vec::new();
    let Ok(rows) = store.list_daemon_settings_with_prefix(MCP_SERVER_SETTING_PREFIX) else {
        return (definitions, unavailable);
    };
    for (key, raw) in rows {
        let Some(id) = key.strip_prefix(MCP_SERVER_SETTING_PREFIX) else {
            continue;
        };
        match parse_definition(id, &raw) {
            Ok(definition) if definition.enabled => {
                definitions.insert(id.to_owned(), definition);
            }
            Ok(_) => {}
            Err(_) => unavailable.push(id.to_owned()),
        }
    }
    (definitions, unavailable)
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(DaemonError::from)
}

fn vault_error(error: VaultError) -> DaemonError {
    match error {
        VaultError::Store(error) => DaemonError::Store(format!("key vault: {error}")),
        error @ (VaultError::InvalidSecret(_)
        | VaultError::InvalidMcpId
        | VaultError::NothingToRotateMcp
        | VaultError::NothingToRotate(_)) => DaemonError::InvalidParam(error.to_string()),
    }
}

fn list(store: &Store, vault: &VaultHandle) -> Result<ListMcpServersResult> {
    let mut servers = Vec::new();
    for (id, definition) in load_definitions(store)? {
        let credential = vault.mcp_metadata(&id);
        servers.push(McpServerSummary {
            definition,
            credential,
        });
    }
    Ok(ListMcpServersResult { servers })
}

fn upsert(
    store: &Store,
    vault: &VaultHandle,
    request: UpsertMcpServerParams,
) -> Result<McpServerSummary> {
    validate_mcp_server_definition(&request.server)
        .map_err(|reason| DaemonError::InvalidParam(reason.into()))?;
    let definitions = load_definitions(store)?;
    if !definitions.contains_key(&request.server.id) && definitions.len() >= MAX_MCP_SERVERS {
        return Err(DaemonError::InvalidParam(
            "the mcp server limit is already full; disable an existing server first".into(),
        ));
    }
    let key = setting_key(&request.server.id);
    let raw = serde_json::to_string(&request.server).map_err(DaemonError::from)?;
    store.set_daemon_setting(&key, &raw)?;
    Ok(McpServerSummary {
        credential: vault.mcp_metadata(&request.server.id),
        definition: request.server,
    })
}

fn set_enabled(
    store: &Store,
    vault: &VaultHandle,
    request: SetMcpServerEnabledParams,
) -> Result<McpServerSummary> {
    validate_mcp_server_id(&request.id)
        .map_err(|reason| DaemonError::InvalidParam(reason.into()))?;
    let key = setting_key(&request.id);
    let raw = store
        .get_daemon_setting(&key)?
        .ok_or_else(|| DaemonError::InvalidParam("mcp server is not configured".into()))?;
    let mut definition = parse_definition(&request.id, &raw)?;
    definition.enabled = request.enabled;
    let raw = serde_json::to_string(&definition).map_err(DaemonError::from)?;
    store.set_daemon_setting(&key, &raw)?;
    Ok(McpServerSummary {
        credential: vault.mcp_metadata(&request.id),
        definition,
    })
}

fn validate_id(id: &str) -> Result<()> {
    validate_mcp_server_id(id)
        .map_err(|_| DaemonError::InvalidParam("invalid mcp server id".into()))
}

fn require_configured_server(store: &Store, id: &str) -> Result<()> {
    store
        .get_daemon_setting(&setting_key(id))?
        .ok_or_else(|| DaemonError::InvalidParam("mcp server is not configured".into()))
        .map(|_| ())
}

/// Dispatch one operator-only MCP configuration RPC. Token-bearing callers are
/// refused before parsing so malformed requests cannot echo secret bytes.
///
/// # Errors
///
/// Returns `PolicyDenied` for attributed callers, `InvalidParam` for invalid
/// requests or unknown/missing servers, and `Store` for persistence failures.
pub fn handle(
    store: &Store,
    vault: &VaultHandle,
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
        METHOD_LIST => {
            parse_empty(method, params)?;
            to_value(&list(store, vault)?)
        }
        METHOD_UPSERT => {
            let request: UpsertMcpServerParams = parse(method, params)?;
            let summary = upsert(store, vault, request)?;
            to_value(&summary)
        }
        METHOD_SET_ENABLED => {
            let request: SetMcpServerEnabledParams = parse(method, params)?;
            let summary = set_enabled(store, vault, request)?;
            to_value(&summary)
        }
        METHOD_SET_SECRET | METHOD_ROTATE_SECRET => {
            let (id, secret) = parse_secret(method, params)?;
            validate_id(&id)?;
            require_configured_server(store, &id)?;
            let metadata = if method == METHOD_SET_SECRET {
                vault.set_mcp(&id, &secret).map_err(vault_error)?
            } else {
                vault.rotate_mcp(&id, &secret).map_err(vault_error)?
            };
            drop(secret);
            to_value(&metadata)
        }
        METHOD_CLEAR_SECRET => {
            let request: McpServerIdParams = parse(method, params)?;
            validate_id(&request.id)?;
            require_configured_server(store, &request.id)?;
            let metadata = vault.clear_mcp(&request.id).map_err(vault_error)?;
            to_value(&metadata)
        }
        other => Err(DaemonError::InvalidParam(format!(
            "unknown mcp configuration method `{other}`"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::vault::{VaultHandleBuilder, VaultSettings};
    use rsi_common::mcp::{
        METHOD_CLEAR_SECRET, METHOD_LIST, METHOD_ROTATE_SECRET, METHOD_SET_ENABLED,
        METHOD_SET_SECRET, METHOD_UPSERT, McpCredentialState,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt as _;

    const SECRET: &str = "mcp-test-secret-788";

    fn definition(id: &str, enabled: bool) -> McpServerDefinition {
        McpServerDefinition {
            id: id.into(),
            command: "/usr/local/bin/mcp-server".into(),
            args: vec!["--stdio".into()],
            secret_env_names: vec!["MCP_SERVER_TOKEN".into()],
            working_dir: Some("/tmp".into()),
            enabled,
        }
    }

    fn upsert_request(id: &str, enabled: bool) -> serde_json::Value {
        serde_json::json!({ "server": definition(id, enabled) })
    }

    struct LogLayer {
        events: Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct FieldVisitor(Vec<String>);

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push(format!("{}={:?}", field.name(), value));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push(format!("{}={}", field.name(), value));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = FieldVisitor(Vec::new());
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0.join(" "));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn config_round_trip_and_secret_flow_are_secret_free() {
        let root = tempfile::tempdir().unwrap();
        let vault_dir = root.path().join("vault");
        let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .dir(&vault_dir)
            .open()
            .unwrap();
        let store = Store::open_in_memory().unwrap();

        let upsert = handle(
            &store,
            &vault,
            METHOD_UPSERT,
            false,
            &upsert_request("docs", false),
        )
        .unwrap();
        assert!(upsert["definition"]["enabled"].is_boolean());
        assert!(!upsert.to_string().contains(SECRET));

        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(LogLayer {
            events: Arc::clone(&logs),
        });
        let set = tracing::subscriber::with_default(subscriber, || {
            handle(
                &store,
                &vault,
                METHOD_SET_SECRET,
                false,
                &serde_json::json!({"id": "docs", "secret": SECRET}),
            )
            .unwrap()
        });
        assert!(!set.to_string().contains(SECRET));
        assert!(
            !logs
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.contains(SECRET))
        );

        let list = handle(&store, &vault, METHOD_LIST, false, &serde_json::Value::Null).unwrap();
        assert!(!list.to_string().contains(SECRET));
        assert_eq!(list["servers"][0]["credential"]["state"], "vault");
        assert_eq!(list["servers"][0]["credential"]["generation"], 1);
        assert!(!format!("{:?}", definition("docs", false)).contains(SECRET));

        let settings = store
            .list_daemon_settings_with_prefix(MCP_SERVER_SETTING_PREFIX)
            .unwrap();
        assert_eq!(settings.len(), 1);
        assert!(!settings[0].1.contains(SECRET));
        let config =
            crate::config::RuntimeConfig::from_config(&crate::config::Config::default()).to_json();
        assert!(!config.to_string().contains(SECRET));

        let vault_path = vault_dir.join("credentials.json");
        let vault_text = fs::read_to_string(&vault_path).unwrap();
        assert!(vault_text.contains(SECRET));
        assert_eq!(
            fs::metadata(&vault_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let rotate = handle(
            &store,
            &vault,
            METHOD_ROTATE_SECRET,
            false,
            &serde_json::json!({"id": "docs", "secret": "mcp-test-rotated-788"}),
        )
        .unwrap();
        assert_eq!(rotate["generation"], 2);
        assert!(rotate["rotated_from_fingerprint"].is_string());

        let cleared = handle(
            &store,
            &vault,
            METHOD_CLEAR_SECRET,
            false,
            &serde_json::json!({"id": "docs"}),
        )
        .unwrap();
        assert_eq!(cleared["state"], "cleared");
        assert_eq!(cleared["generation"], 3);
        let vault_text = fs::read_to_string(&vault_path).unwrap();
        assert!(vault_text.contains("\"mcp_cleared\""));
        assert!(!vault_text.contains(SECRET));
        assert!(!vault_text.contains("mcp-test-rotated-788"));

        let metadata = vault.mcp_metadata("docs");
        assert_eq!(metadata.state, McpCredentialState::Cleared);
        assert_eq!(metadata.generation, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn validation_and_enable_switch_are_bounded_and_secret_free() {
        let root = tempfile::tempdir().unwrap();
        let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .dir(root.path().join("vault"))
            .open()
            .unwrap();
        let store = Store::open_in_memory().unwrap();
        handle(
            &store,
            &vault,
            METHOD_UPSERT,
            false,
            &upsert_request("docs", false),
        )
        .unwrap();
        let enabled = handle(
            &store,
            &vault,
            METHOD_SET_ENABLED,
            false,
            &serde_json::json!({"id": "docs", "enabled": true}),
        )
        .unwrap();
        assert_eq!(enabled["definition"]["enabled"], true);

        let error = handle(
            &store,
            &vault,
            METHOD_UPSERT,
            false,
            &serde_json::json!({
                "server": {
                    "id": "Docs",
                    "command": "mcp --shell",
                    "args": [],
                    "secret_env_names": [],
                    "working_dir": null,
                    "enabled": false
                }
            }),
        )
        .unwrap_err();
        assert!(!error.to_string().contains(SECRET));

        let error = handle(
            &store,
            &vault,
            METHOD_SET_SECRET,
            false,
            &serde_json::json!({"id": SECRET, "secret": SECRET}),
        )
        .unwrap_err();
        assert!(!error.to_string().contains(SECRET));

        let oversized = "x".repeat(crate::vault::MAX_SECRET_BYTES + 1);
        let error = handle(
            &store,
            &vault,
            METHOD_SET_SECRET,
            false,
            &serde_json::json!({"id": "docs", "secret": oversized}),
        )
        .unwrap_err();
        assert!(!error.to_string().contains(&oversized));

        for method in [METHOD_SET_SECRET, METHOD_CLEAR_SECRET] {
            let error = handle(
                &store,
                &vault,
                method,
                false,
                &serde_json::json!({"id": "not-configured", "secret": SECRET}),
            )
            .unwrap_err();
            assert!(matches!(error, DaemonError::InvalidParam(_)), "{method}");
            assert!(!error.to_string().contains(SECRET), "{method}");
        }

        for method in [
            METHOD_LIST,
            METHOD_UPSERT,
            METHOD_SET_ENABLED,
            METHOD_SET_SECRET,
            METHOD_ROTATE_SECRET,
            METHOD_CLEAR_SECRET,
        ] {
            let error = handle(
                &store,
                &vault,
                method,
                true,
                &serde_json::json!({"id": "docs", "secret": SECRET}),
            )
            .unwrap_err();
            assert!(
                matches!(&error, DaemonError::PolicyDenied(message) if message.contains(OPERATOR_ONLY_REFUSAL)),
                "{method}: {error}"
            );
            assert!(!error.to_string().contains(SECRET));
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn server_count_is_capped() {
        let root = tempfile::tempdir().unwrap();
        let vault = VaultHandleBuilder::new(Arc::new(VaultSettings::default()))
            .dir(root.path().join("vault"))
            .open()
            .unwrap();
        let store = Store::open_in_memory().unwrap();
        for index in 0..MAX_MCP_SERVERS {
            handle(
                &store,
                &vault,
                METHOD_UPSERT,
                false,
                &upsert_request(&format!("server-{index:02}"), false),
            )
            .unwrap();
        }
        let error = handle(
            &store,
            &vault,
            METHOD_UPSERT,
            false,
            &upsert_request("server-32", false),
        )
        .unwrap_err();
        assert!(matches!(error, DaemonError::InvalidParam(_)));
    }
}
