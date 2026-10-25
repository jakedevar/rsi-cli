//! Custom OpenAI-compatible providers (hub #3039).
//!
//! Storage, no schema change:
//! - one nonsecret JSON row per provider under `custom_provider.<uuid>` in
//!   `daemon_settings` (name, base URL, default model, list ordinal). A delete
//!   writes `deleted_at` into the row instead of removing it (no hard delete),
//!   which also keeps the one-time import from resurrecting it;
//! - the API key in the key vault's custom-provider file
//!   (`~/.rsi/vault/custom-provider-keys.json`, 0600), never in a row, a
//!   response, a log or an event. Reads carry `api_key.state` and the vault
//!   fingerprint only;
//! - model-role assignments under `custom_provider_route.<role>` (a UUID or
//!   `null`). The daemon writes the role's `<prefix>_base_url` and
//!   in-memory `<prefix>_api_key` itself, at assignment, on every change to
//!   the provider and at daemon start, so no frontend ever holds the key;
//! - the one-time import marker `custom_provider_import.v1`
//!   ([`super::legacy_import`]).

use super::OperatorConfigContext;
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::vault::{VaultError, VaultHandle};
use chrono::Utc;
use rsi_common::operator_config::{
    CustomProviderIdParams, CustomProviderKeyChange, CustomProviderRole, CustomProviderRouteResult,
    CustomProviderRouteView, CustomProviderSummary, DeleteCustomProviderResult,
    ListCustomProviderRoutesResult, ListCustomProvidersResult, MAX_CUSTOM_PROVIDER_MODEL_BYTES,
    MAX_CUSTOM_PROVIDER_NAME_BYTES, MAX_CUSTOM_PROVIDER_URL_BYTES, MAX_CUSTOM_PROVIDERS,
    PutCustomProviderParams, SetCustomProviderRouteParams,
};
use uuid::Uuid;

pub(super) use crate::store::operator_settings::StoredCustomProvider;

fn vault_error(error: VaultError) -> DaemonError {
    match error {
        VaultError::Store(error) => DaemonError::Store(format!("key vault: {error}")),
        error => DaemonError::InvalidParam(error.to_string()),
    }
}

pub(super) fn single_line(field: &str, value: &str, max: usize, required: bool) -> Result<String> {
    let trimmed = value.trim();
    if required && trimmed.is_empty() {
        return Err(DaemonError::InvalidParam(format!("{field} is required")));
    }
    if trimmed.len() > max {
        return Err(DaemonError::InvalidParam(format!(
            "{field} is longer than {max} bytes"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(DaemonError::InvalidParam(format!(
            "{field} must be a single line without control characters"
        )));
    }
    Ok(trimmed.to_owned())
}

/// An `http(s)://` URL with no whitespace and no credentials in it (a key
/// belongs in the vault, never in a stored row).
pub(super) fn validate_base_url(value: &str) -> Result<String> {
    let url = single_line("base_url", value, MAX_CUSTOM_PROVIDER_URL_BYTES, true)?;
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| {
            DaemonError::InvalidParam("base_url must start with http:// or https://".into())
        })?;
    if url.chars().any(char::is_whitespace) {
        return Err(DaemonError::InvalidParam(
            "base_url must not contain whitespace".into(),
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(DaemonError::InvalidParam(
            "base_url needs a host and must not carry credentials; set the key as api_key".into(),
        ));
    }
    Ok(url)
}

/// Every row, deleted ones included, in list order.
pub(super) fn load_rows(store: &Store) -> Result<Vec<StoredCustomProvider>> {
    let mut rows = store.list_custom_provider_records()?;
    rows.sort_by(|left, right| (left.ordinal, left.id).cmp(&(right.ordinal, right.id)));
    Ok(rows)
}

fn load_row(store: &Store, id: Uuid) -> Result<Option<StoredCustomProvider>> {
    store.get_custom_provider_record(id)
}

fn load_live(store: &Store, id: Uuid) -> Result<StoredCustomProvider> {
    load_row(store, id)?
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(|| DaemonError::InvalidParam("custom provider is not configured".into()))
}

pub(super) fn save_row(store: &Store, row: &StoredCustomProvider) -> Result<()> {
    store.set_custom_provider_record(row)
}

fn summary(vault: &VaultHandle, row: StoredCustomProvider) -> CustomProviderSummary {
    CustomProviderSummary {
        api_key: vault.custom_provider_key_metadata(row.id),
        id: row.id,
        name: row.name,
        base_url: row.base_url,
        default_model: row.default_model,
    }
}

/// `ListCustomProviders`.
///
/// # Errors
///
/// `Store` for a read failure or a malformed row.
pub fn list(store: &Store, vault: &VaultHandle) -> Result<ListCustomProvidersResult> {
    Ok(ListCustomProvidersResult {
        providers: load_rows(store)?
            .into_iter()
            .filter(|row| row.deleted_at.is_none())
            .map(|row| summary(vault, row))
            .collect(),
    })
}

/// `GetCustomProvider`.
///
/// # Errors
///
/// `InvalidParam` for an unknown provider.
pub fn get(
    context: &OperatorConfigContext<'_>,
    request: CustomProviderIdParams,
) -> Result<CustomProviderSummary> {
    Ok(summary(
        context.vault,
        load_live(context.store, request.id)?,
    ))
}

/// `PutCustomProvider`.
///
/// # Errors
///
/// `InvalidParam` for invalid fields, an invalid key, an unknown id or a
/// full list; `Store` for a persistence failure.
pub fn put(
    context: &OperatorConfigContext<'_>,
    request: PutCustomProviderParams,
) -> Result<CustomProviderSummary> {
    let PutCustomProviderParams {
        id,
        name,
        base_url,
        default_model,
        api_key,
    } = request;
    let name = single_line("name", &name, MAX_CUSTOM_PROVIDER_NAME_BYTES, true)?;
    let base_url = validate_base_url(&base_url)?;
    let default_model = single_line(
        "default_model",
        &default_model,
        MAX_CUSTOM_PROVIDER_MODEL_BYTES,
        false,
    )?;
    let secret = match api_key {
        CustomProviderKeyChange::Set { secret } => {
            let secret = zeroize::Zeroizing::new(secret);
            crate::vault::validate_secret_input(&secret).map_err(vault_error)?;
            Some(secret)
        }
        CustomProviderKeyChange::Keep => None,
        CustomProviderKeyChange::Clear => {
            if let Some(id) = id {
                load_live(context.store, id)?;
                context
                    .vault
                    .clear_custom_provider_key(id)
                    .map_err(vault_error)?;
            }
            None
        }
    };

    let rows = load_rows(context.store)?;
    let row = match id {
        Some(id) => {
            let existing = rows
                .iter()
                .find(|row| row.id == id && row.deleted_at.is_none())
                .ok_or_else(|| {
                    DaemonError::InvalidParam("custom provider is not configured".into())
                })?;
            StoredCustomProvider {
                name,
                base_url,
                default_model,
                ..existing.clone()
            }
        }
        None => {
            if rows.iter().filter(|row| row.deleted_at.is_none()).count() >= MAX_CUSTOM_PROVIDERS {
                return Err(DaemonError::InvalidParam(format!(
                    "the custom provider list is full ({MAX_CUSTOM_PROVIDERS}); delete one first"
                )));
            }
            StoredCustomProvider {
                id: Uuid::new_v4(),
                name,
                base_url,
                default_model,
                ordinal: next_ordinal(&rows),
                deleted_at: None,
            }
        }
    };
    save_row(context.store, &row)?;
    if let Some(secret) = secret
        && let Err(error) = context.vault.set_custom_provider_key(row.id, &secret)
    {
        if id.is_none() {
            // A new provider whose key could not be stored is withdrawn
            // (tombstoned) so a retry does not leave a keyless duplicate.
            let mut withdrawn = row.clone();
            withdrawn.deleted_at = Some(Utc::now());
            let _ = save_row(context.store, &withdrawn);
        }
        return Err(vault_error(error));
    }
    reapply_routes_for(context.store, context.vault, context.runtime_config, row.id)?;
    Ok(summary(context.vault, row))
}

pub(super) fn next_ordinal(rows: &[StoredCustomProvider]) -> u64 {
    rows.iter()
        .map(|row| row.ordinal.saturating_add(1))
        .max()
        .unwrap_or(0)
}

/// `DeleteCustomProvider`: tombstone the row, remove the vault key and clear
/// every model role that pointed at it.
///
/// # Errors
///
/// `InvalidParam` for an unknown provider; `Store` for a persistence failure.
pub fn delete(
    context: &OperatorConfigContext<'_>,
    request: CustomProviderIdParams,
) -> Result<DeleteCustomProviderResult> {
    let mut row = load_live(context.store, request.id)?;
    row.deleted_at = Some(Utc::now());
    save_row(context.store, &row)?;
    context
        .vault
        .clear_custom_provider_key(row.id)
        .map_err(vault_error)?;
    let mut cleared_routes = Vec::new();
    for role in CustomProviderRole::ALL {
        if route_of(context.store, role)? == Some(row.id) {
            store_route(context.store, role, None)?;
            apply_route(
                context.store,
                context.vault,
                context.runtime_config,
                role,
                None,
            )?;
            cleared_routes.push(role);
        }
    }
    Ok(DeleteCustomProviderResult {
        id: row.id,
        deleted: true,
        cleared_routes,
    })
}

fn route_of(store: &Store, role: CustomProviderRole) -> Result<Option<Uuid>> {
    store.get_custom_provider_route(role)
}

fn store_route(store: &Store, role: CustomProviderRole, id: Option<Uuid>) -> Result<()> {
    store.set_custom_provider_route(role, id)
}

/// Write one role's base URL (persisted, like `UpdateDaemonConfig`) and its
/// in-memory key from the provider row and the vault. `None` clears both.
/// Returns whether a key is now set.
fn apply_route(
    store: &Store,
    vault: &VaultHandle,
    runtime_config: &RuntimeConfig,
    role: CustomProviderRole,
    id: Option<Uuid>,
) -> Result<bool> {
    let provider = match id {
        Some(id) => load_row(store, id)?.filter(|row| row.deleted_at.is_none()),
        None => None,
    };
    let base_url = provider.as_ref().map_or(serde_json::Value::Null, |row| {
        serde_json::Value::String(row.base_url.clone())
    });
    let secret = provider
        .as_ref()
        .and_then(|row| vault.custom_provider_key(row.id));
    let api_key_set = secret.is_some();
    let prefix = role.config_prefix();
    let base_url_field = format!("{prefix}_base_url");
    runtime_config
        .update_field(&base_url_field, &base_url)
        .map_err(|error| DaemonError::Store(format!("{base_url_field}: {error}")))?;
    crate::store::daemon_settings::persist_runtime_config_field(
        store,
        runtime_config,
        &base_url_field,
    )?;
    let api_key_field = format!("{prefix}_api_key");
    let key_value = secret.map_or(serde_json::Value::Null, |secret| {
        serde_json::Value::String(secret.expose().to_owned())
    });
    // The key is in-memory only: `<prefix>_api_key` is not a persisted field.
    runtime_config
        .update_field(&api_key_field, &key_value)
        .map_err(|_| DaemonError::Store(format!("{api_key_field} could not be updated")))?;
    drop(key_value);
    Ok(api_key_set)
}

/// `SetCustomProviderRoute`.
///
/// # Errors
///
/// `InvalidParam` for an unknown provider; `Store` for a persistence failure.
pub fn set_route(
    context: &OperatorConfigContext<'_>,
    request: SetCustomProviderRouteParams,
) -> Result<CustomProviderRouteResult> {
    if let Some(id) = request.id {
        load_live(context.store, id)?;
    }
    store_route(context.store, request.role, request.id)?;
    let api_key_set = apply_route(
        context.store,
        context.vault,
        context.runtime_config,
        request.role,
        request.id,
    )?;
    Ok(CustomProviderRouteResult {
        role: request.role,
        id: request.id,
        api_key_set,
    })
}

/// `ListCustomProviderRoutes`: the stored route of every model role, read
/// through the accessors `SetCustomProviderRoute` writes with. Key metadata
/// only (state, fingerprint); the key never leaves the vault.
///
/// # Errors
///
/// `Store` for a read failure or a malformed row.
pub fn list_routes(store: &Store, vault: &VaultHandle) -> Result<ListCustomProviderRoutesResult> {
    let mut routes = Vec::with_capacity(CustomProviderRole::ALL.len());
    for role in CustomProviderRole::ALL {
        let provider = match route_of(store, role)? {
            Some(id) => load_row(store, id)?.filter(|row| row.deleted_at.is_none()),
            None => None,
        };
        routes.push(match provider {
            Some(row) => CustomProviderRouteView {
                role,
                id: Some(row.id),
                api_key: Some(vault.custom_provider_key_metadata(row.id)),
                name: Some(row.name),
            },
            None => CustomProviderRouteView {
                role,
                id: None,
                name: None,
                api_key: None,
            },
        });
    }
    Ok(ListCustomProviderRoutesResult { routes })
}

fn reapply_routes_for(
    store: &Store,
    vault: &VaultHandle,
    runtime_config: &RuntimeConfig,
    id: Uuid,
) -> Result<()> {
    for role in CustomProviderRole::ALL {
        if route_of(store, role)? == Some(id) {
            apply_route(store, vault, runtime_config, role, Some(id))?;
        }
    }
    Ok(())
}

/// Daemon start: reapply every stored role assignment (the key lives only in
/// memory). A role with no assignment row is left as the persisted settings
/// put it. Returns the number of roles applied.
///
/// # Errors
///
/// `Store` for a read or persistence failure.
pub fn reapply_all_routes(
    store: &Store,
    vault: &VaultHandle,
    runtime_config: &RuntimeConfig,
) -> Result<usize> {
    let mut applied = 0;
    for role in CustomProviderRole::ALL {
        if let Some(id) = route_of(store, role)? {
            apply_route(store, vault, runtime_config, role, Some(id))?;
            applied += 1;
        }
    }
    Ok(applied)
}

/// The daemon-side key and base URL for a launch or a text request that
/// names a custom provider, so a frontend sends only the id.
///
/// # Errors
///
/// `InvalidParam` for an unknown provider.
pub fn resolve_for_request(
    store: &Store,
    vault: &VaultHandle,
    id: Uuid,
) -> Result<(String, Option<crate::vault::SecretString>)> {
    let row = load_live(store, id)?;
    Ok((row.base_url, vault.custom_provider_key(id)))
}
