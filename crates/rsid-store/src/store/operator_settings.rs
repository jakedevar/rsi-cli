//! Typed persistence for operator-owned MCP and custom-provider settings (#457).
//!
//! Keep the existing keys and JSON representation. Validation and vault custody
//! remain with the operator RPC handlers; these rows contain no credentials.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::mcp::McpServerDefinition;
use rsi_common::operator_config::CustomProviderRole;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MCP_SERVER_SETTING_PREFIX: &str = "mcp.server.";
const CUSTOM_PROVIDER_SETTING_PREFIX: &str = "custom_provider.";
const CUSTOM_PROVIDER_ROUTE_PREFIX: &str = "custom_provider_route.";

/// The nonsecret row. There is deliberately no field that could hold a key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCustomProvider {
    pub id: Uuid,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
    pub ordinal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
}

/// A session's nonsecret Provider Setup reference and original route. The
/// daemon resolves the credential from the referenced vault slot at launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCustomProviderBinding {
    pub custom_provider_id: Uuid,
    pub base_url: String,
    pub provider: rsi_common::types::SessionProvider,
    pub uses_saved_credential: bool,
    pub requires_credential: bool,
    pub uses_saved_endpoint: bool,
}

fn session_custom_provider_key(session_id: Uuid) -> String {
    format!("session_custom_provider.{session_id}")
}

fn mcp_server_key(id: &str) -> String {
    format!("{MCP_SERVER_SETTING_PREFIX}{id}")
}

fn custom_provider_key(id: Uuid) -> String {
    format!("{CUSTOM_PROVIDER_SETTING_PREFIX}{}", id.as_hyphenated())
}

fn custom_provider_route_key(role: CustomProviderRole) -> String {
    format!("{CUSTOM_PROVIDER_ROUTE_PREFIX}{}", role.as_str())
}

fn parse_custom_provider(key: &str, raw: &str) -> Result<StoredCustomProvider> {
    let row: StoredCustomProvider = serde_json::from_str(raw)
        .map_err(|_| DaemonError::Store(format!("stored custom provider {key} is malformed")))?;
    if custom_provider_key(row.id) != key {
        return Err(DaemonError::Store(format!(
            "stored custom provider {key} has a mismatched id"
        )));
    }
    Ok(row)
}

impl Store {
    pub fn set_session_custom_provider_binding(
        &self,
        session_id: Uuid,
        binding: &SessionCustomProviderBinding,
    ) -> Result<()> {
        self.set_daemon_setting(
            &session_custom_provider_key(session_id),
            &serde_json::to_string(binding)?,
        )
    }

    pub fn resolve_session_custom_provider_binding(
        &self,
        mut session_id: Uuid,
    ) -> Result<Option<SessionCustomProviderBinding>> {
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(session_id) {
                return Err(DaemonError::Store("custom_provider_lineage_cycle".into()));
            }
            if let Some(raw) = self.get_daemon_setting(&session_custom_provider_key(session_id))? {
                return serde_json::from_str(&raw)
                    .map(Some)
                    .map_err(|_| DaemonError::Store("custom_provider_binding_unreadable".into()));
            }
            match self
                .get_session(session_id)?
                .and_then(|row| row.continued_from)
            {
                Some(parent) => session_id = parent,
                None => return Ok(None),
            }
        }
    }

    /// Keep raw reads so MCP's strict and lossy loaders retain their distinct
    /// handling of malformed definitions.
    pub fn get_mcp_server_definition_raw(&self, id: &str) -> Result<Option<String>> {
        self.get_daemon_setting(&mcp_server_key(id))
    }

    pub fn list_mcp_server_definition_rows(&self) -> Result<Vec<(String, String)>> {
        self.list_daemon_settings_with_prefix(MCP_SERVER_SETTING_PREFIX)
    }

    /// Persist an already validated nonsecret definition under its canonical id.
    pub fn set_mcp_server_definition(&self, definition: &McpServerDefinition) -> Result<()> {
        self.set_daemon_setting(
            &mcp_server_key(&definition.id),
            &serde_json::to_string(definition)?,
        )
    }

    /// Includes tombstones, which prevent the one-time import resurrecting a
    /// deleted provider. The caller retains responsibility for list ordering.
    pub fn list_custom_provider_records(&self) -> Result<Vec<StoredCustomProvider>> {
        self.list_daemon_settings_with_prefix(CUSTOM_PROVIDER_SETTING_PREFIX)?
            .into_iter()
            .map(|(key, raw)| parse_custom_provider(&key, &raw))
            .collect()
    }

    pub fn get_custom_provider_record(&self, id: Uuid) -> Result<Option<StoredCustomProvider>> {
        let key = custom_provider_key(id);
        self.get_daemon_setting(&key)?
            .map(|raw| parse_custom_provider(&key, &raw))
            .transpose()
    }

    pub fn set_custom_provider_record(&self, row: &StoredCustomProvider) -> Result<()> {
        self.set_daemon_setting(&custom_provider_key(row.id), &serde_json::to_string(row)?)
    }

    pub fn get_custom_provider_route(&self, role: CustomProviderRole) -> Result<Option<Uuid>> {
        Ok(self
            .get_daemon_setting(&custom_provider_route_key(role))?
            .and_then(|raw| Uuid::parse_str(raw.trim()).ok()))
    }

    pub fn set_custom_provider_route(
        &self,
        role: CustomProviderRole,
        id: Option<Uuid>,
    ) -> Result<()> {
        let raw = id.map_or_else(|| "null".to_owned(), |id| id.as_hyphenated().to_string());
        self.set_daemon_setting(&custom_provider_route_key(role), &raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn session_custom_provider_binding_round_trips_and_rejects_damaged_records() {
        let store = Store::open_in_memory().unwrap();
        let session_id = Uuid::new_v4();
        let binding = SessionCustomProviderBinding {
            custom_provider_id: Uuid::new_v4(),
            base_url: "http://localhost:1234/v1".into(),
            provider: rsi_common::types::SessionProvider::Harness,
            uses_saved_credential: true,
            requires_credential: true,
            uses_saved_endpoint: true,
        };
        assert_eq!(
            store
                .resolve_session_custom_provider_binding(session_id)
                .unwrap(),
            None
        );
        store
            .set_session_custom_provider_binding(session_id, &binding)
            .unwrap();
        assert_eq!(
            store
                .resolve_session_custom_provider_binding(session_id)
                .unwrap(),
            Some(binding.clone())
        );
        assert_eq!(
            store
                .get_daemon_setting(&session_custom_provider_key(session_id))
                .unwrap(),
            Some(serde_json::to_string(&binding).unwrap())
        );
        store
            .set_daemon_setting(&session_custom_provider_key(session_id), "{}")
            .unwrap();
        assert!(
            store
                .resolve_session_custom_provider_binding(session_id)
                .unwrap_err()
                .to_string()
                .contains("custom_provider_binding_unreadable")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn mcp_accessor_preserves_keys_json_and_enable_updates() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.get_mcp_server_definition_raw("docs").unwrap(), None);
        let mut definition = McpServerDefinition {
            id: "docs".into(),
            command: "/usr/bin/docs".into(),
            args: vec!["--stdio".into()],
            secret_env_names: vec!["DOCS_TOKEN".into()],
            working_dir: None,
            enabled: false,
        };
        store.set_mcp_server_definition(&definition).unwrap();
        let expected = serde_json::to_string(&definition).unwrap();
        assert_eq!(
            store.get_daemon_setting("mcp.server.docs").unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            store.list_mcp_server_definition_rows().unwrap(),
            vec![("mcp.server.docs".into(), expected)]
        );
        definition.enabled = true;
        store.set_mcp_server_definition(&definition).unwrap();
        assert_eq!(
            serde_json::from_str::<McpServerDefinition>(
                &store
                    .get_mcp_server_definition_raw("docs")
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            definition
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn custom_provider_accessor_preserves_defaults_tombstones_and_json() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        assert_eq!(store.get_custom_provider_record(id).unwrap(), None);
        let key = format!("custom_provider.{id}");
        store.set_daemon_setting(&key, &serde_json::json!({"id":id,"name":"local","base_url":"http://localhost:1234/v1","ordinal":3}).to_string()).unwrap();
        let mut row = store.get_custom_provider_record(id).unwrap().unwrap();
        assert_eq!(row.default_model, "");
        assert_eq!(row.deleted_at, None);
        store.set_custom_provider_record(&row).unwrap();
        assert_eq!(
            store.get_daemon_setting(&key).unwrap(),
            Some(serde_json::to_string(&row).unwrap())
        );
        row.deleted_at = Some(Utc::now());
        store.set_custom_provider_record(&row).unwrap();
        assert_eq!(
            store.get_custom_provider_record(id).unwrap(),
            Some(row.clone())
        );
        assert_eq!(store.list_custom_provider_records().unwrap(), vec![row]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn custom_provider_routes_preserve_missing_null_uuid_and_invalid_defaults() {
        let store = Store::open_in_memory().unwrap();
        for role in CustomProviderRole::ALL {
            let id = Uuid::new_v4();
            let key = format!("custom_provider_route.{}", role.as_str());
            assert_eq!(store.get_custom_provider_route(role).unwrap(), None);
            store.set_custom_provider_route(role, Some(id)).unwrap();
            assert_eq!(store.get_custom_provider_route(role).unwrap(), Some(id));
            assert_eq!(
                store.get_daemon_setting(&key).unwrap(),
                Some(id.to_string())
            );
            store.set_custom_provider_route(role, None).unwrap();
            assert_eq!(
                store.get_daemon_setting(&key).unwrap().as_deref(),
                Some("null")
            );
            assert_eq!(store.get_custom_provider_route(role).unwrap(), None);
            store.set_daemon_setting(&key, "invalid").unwrap();
            assert_eq!(store.get_custom_provider_route(role).unwrap(), None);
            store.set_daemon_setting(&key, &format!(" {id} ")).unwrap();
            assert_eq!(store.get_custom_provider_route(role).unwrap(), Some(id));
        }
    }
}
