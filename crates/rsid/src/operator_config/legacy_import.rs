//! One-time import of the TUI's legacy custom providers (hub #3039).
//!
//! Before #3039 the TUI kept `settings.custom_providers` in its
//! `~/.rsi/state.json`, each entry with a plaintext `api_key`. At daemon start
//! this copies them into the store rows of [`super::custom_providers`] and
//! their keys into the vault, once. The file is only read: never written,
//! renamed or deleted, so an older TUI keeps working from it unchanged.

use super::OperatorConfigPaths;
use super::custom_providers::{
    StoredCustomProvider, load_rows, next_ordinal, save_row, single_line, validate_base_url,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::vault::{VaultError, VaultHandle};
use chrono::Utc;
use rsi_common::operator_config::{
    CustomProviderKeyState, MAX_CUSTOM_PROVIDER_MODEL_BYTES, MAX_CUSTOM_PROVIDER_NAME_BYTES,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The `daemon_settings` row that records the import ran.
pub const LEGACY_IMPORT_MARKER: &str = "custom_provider_import.v1";

/// What the one-time import did. Counts and ids only, never a key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyImportReport {
    /// The marker was already present: nothing was read or written.
    pub already_imported: bool,
    pub source_present: bool,
    pub providers_imported: usize,
    pub keys_imported: usize,
    pub providers_skipped: usize,
    pub keys_skipped: usize,
}

/// The legacy `CustomProviderEntry` shape in the TUI's `state.json`.
#[derive(Deserialize)]
struct LegacyEntry {
    id: Uuid,
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    default_model: String,
}

/// Copy the TUI's legacy `settings.custom_providers` (and their plaintext
/// keys) from `state.json` into the store and the vault, once.
///
/// Read-only on the file: it is never written, renamed or deleted, so an
/// older TUI keeps reading it unchanged. Idempotent: after the marker row is
/// written nothing runs again, and before it every provider whose row exists
/// (live or deleted) and every key the vault already holds is skipped, so a
/// crash midway converges on the next start.
///
/// # Errors
///
/// `Store` for a store or vault write failure. An unreadable or malformed
/// file imports nothing and still writes the marker with
/// `source_present: false`.
pub fn import_legacy_tui_state(
    store: &Store,
    vault: &VaultHandle,
    paths: &OperatorConfigPaths,
) -> Result<LegacyImportReport> {
    if store.get_daemon_setting(LEGACY_IMPORT_MARKER)?.is_some() {
        return Ok(LegacyImportReport {
            already_imported: true,
            ..LegacyImportReport::default()
        });
    }
    let report = import_entries(store, vault, paths)?;
    let marker = serde_json::json!({
        "imported_at": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        "source_present": report.source_present,
        "providers_imported": report.providers_imported,
        "keys_imported": report.keys_imported,
        "providers_skipped": report.providers_skipped,
        "keys_skipped": report.keys_skipped,
    });
    store.set_daemon_setting(LEGACY_IMPORT_MARKER, &marker.to_string())?;
    Ok(report)
}

/// The import step before the marker is written (a crash between the two
/// reruns this on the next start, which must converge).
pub(super) fn import_entries(
    store: &Store,
    vault: &VaultHandle,
    paths: &OperatorConfigPaths,
) -> Result<LegacyImportReport> {
    let mut report = LegacyImportReport::default();
    let entries = match std::fs::read(&paths.legacy_tui_state) {
        Ok(bytes) => {
            let bytes = zeroize::Zeroizing::new(bytes);
            legacy_entries(&bytes).inspect(|_| report.source_present = true)
        }
        Err(_) => None,
    };
    if let Some(entries) = entries {
        let mut rows = load_rows(store)?;
        for entry in entries {
            let Some(entry) = entry else {
                report.providers_skipped += 1;
                continue;
            };
            let api_key = zeroize::Zeroizing::new(entry.api_key);
            let exists = rows.iter().any(|row| row.id == entry.id);
            if !exists {
                let fields = (
                    single_line("name", &entry.name, MAX_CUSTOM_PROVIDER_NAME_BYTES, true),
                    validate_base_url(&entry.base_url),
                    single_line(
                        "default_model",
                        &entry.default_model,
                        MAX_CUSTOM_PROVIDER_MODEL_BYTES,
                        false,
                    ),
                );
                let (Ok(name), Ok(base_url), Ok(default_model)) = fields else {
                    report.providers_skipped += 1;
                    continue;
                };
                let row = StoredCustomProvider {
                    id: entry.id,
                    name,
                    base_url,
                    default_model,
                    ordinal: next_ordinal(&rows),
                    deleted_at: None,
                };
                save_row(store, &row)?;
                rows.push(row);
                report.providers_imported += 1;
            }
            let live = rows
                .iter()
                .any(|row| row.id == entry.id && row.deleted_at.is_none());
            if !live || api_key.trim().is_empty() {
                continue;
            }
            if vault.custom_provider_key_metadata(entry.id).state == CustomProviderKeyState::Set {
                continue;
            }
            match vault.set_custom_provider_key(entry.id, &api_key) {
                Ok(_) => report.keys_imported += 1,
                Err(VaultError::Store(error)) => {
                    return Err(DaemonError::Store(format!("key vault: {error}")));
                }
                Err(_) => report.keys_skipped += 1,
            }
        }
    }
    Ok(report)
}

/// `settings.custom_providers` of a `state.json`, one `None` per entry that
/// does not parse. `None` overall when the file is not JSON or has no list.
fn legacy_entries(bytes: &[u8]) -> Option<Vec<Option<LegacyEntry>>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let list = value.get("settings")?.get("custom_providers")?.as_array()?;
    Some(
        list.iter()
            .map(|entry| serde_json::from_value(entry.clone()).ok())
            .collect(),
    )
}
