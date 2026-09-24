//! RSI-026 — end-to-end persistence test for `system_prompt_preset`.
//!
//! Validates the daemon-side write-through that
//! `handle_update_daemon_config` performs after a successful
//! `RuntimeConfig::update_field` call. This is the closest test to a true
//! RPC round-trip without standing up a subprocess: it exercises the exact
//! sequence the handler runs, including the SQLite UPSERT, and proves that
//! a daemon "restart" (drop+reopen Store, build new RuntimeConfig) surfaces
//! the persisted value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rsid::config::{Config, RuntimeConfig};
use rsid::rpc::UpdateDaemonConfigParams;
use rsid::store::Store;
use rsid::store::daemon_settings::{
    KEY_SYSTEM_PROMPT_PRESET, apply_persisted_runtime_config,
    maybe_import_legacy_system_prompt_preset, persist_runtime_config_field,
};
use tempfile::TempDir;

/// Helper: simulate the exact sequence in
/// `RpcServer::handle_update_daemon_config`: validate/mutate RuntimeConfig,
/// then write durable daemon settings through SQLite when the field is
/// configured as persistent. Returns the field value from GetDaemonConfig's
/// runtime snapshot after the write-through completes.
fn apply_update_via_handler_path(
    rc: &RuntimeConfig,
    store: &Store,
    field: &str,
    value: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let params: UpdateDaemonConfigParams = serde_json::from_value(serde_json::json!({
        "field": field,
        "value": value,
    }))
    .unwrap();

    let updated = rc
        .update_field(&params.field, &params.value)
        .map_err(|e| format!("update_field rejected: {e}"))?;
    if !updated {
        return Err(format!("update_field returned Ok(false) for {field}"));
    }

    // Write-through to SQLite — mirrors handle_update_daemon_config.
    if rsid::config::is_persisted_runtime_config_field(&params.field) {
        persist_runtime_config_field(store, rc, &params.field)
            .map_err(|e| format!("persist_runtime_config_field failed: {e}"))?;
    }

    rc.to_json()
        .get(&params.field)
        .cloned()
        .ok_or_else(|| format!("field missing from runtime snapshot: {}", params.field))
}

/// End-to-end persistence — cycle the preset, simulate daemon restart, verify
/// the new value is the one we see post-restart (no state.json involved).
#[test]
fn update_daemon_config_then_restart_returns_persisted_value() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let state_path = db_dir.path().join("state.json");
    // No state.json — clean install.

    // --- Boot 1 ---
    let seed = {
        let store = Store::open(&db_path).unwrap();
        let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
        assert_eq!(seed, "default");

        let config = Config::from_env();
        let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed.clone());
        assert_eq!(*rc.system_prompt_preset.read(), "default");

        // Apply the update via the same path the RPC handler takes.
        let stored =
            apply_update_via_handler_path(&rc, &store, "system_prompt_preset", "caveman".into())
                .expect("apply update");
        assert_eq!(stored.as_str(), Some("caveman"));

        // Confirm SQLite row.
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some("caveman"));
        seed
    }; // store + rc drop, simulating daemon shutdown.
    let _ = seed;

    // --- Boot 2 ---
    {
        let store = Store::open(&db_path).unwrap();
        // First call: idempotent — should return the stored value, not re-read state.json.
        let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
        assert_eq!(
            seed, "caveman",
            "daemon restart must surface the persisted value, not the boot-1 default"
        );
        let config = Config::from_env();
        let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed);
        let json = rc.to_json();
        assert_eq!(
            json.get("system_prompt_preset").and_then(|v| v.as_str()),
            Some("caveman")
        );
    }
}

/// Invalid value via the handler path: update_field rejects, no SQLite write.
#[test]
fn update_daemon_config_rejects_garbage_no_sqlite_write() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let store = Store::open(&db_path).unwrap();
    let state_path = db_dir.path().join("state.json");
    let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
    let config = Config::from_env();
    let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed);

    let err = apply_update_via_handler_path(&rc, &store, "system_prompt_preset", "garbage".into())
        .expect_err("garbage value must be rejected");
    assert!(
        err.contains("default") || err.contains("concise"),
        "rejection message must enumerate valid options: {err}"
    );

    // SQLite row must still be the seed default; the rejection short-circuited
    // before set_daemon_setting.
    let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
    assert_eq!(
        row.as_deref(),
        Some("default"),
        "rejected update must NOT write a partial value"
    );
}

/// Round-trip every canonical slug through the handler path.
#[test]
fn each_canonical_slug_persists_through_handler_path() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let store = Store::open(&db_path).unwrap();
    let state_path = db_dir.path().join("state.json");
    let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
    let config = Config::from_env();
    let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed);

    for slug in ["default", "concise", "code-only", "caveman"] {
        let stored =
            apply_update_via_handler_path(&rc, &store, "system_prompt_preset", slug.into())
                .expect("apply update");
        assert_eq!(stored.as_str(), Some(slug));
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some(slug));
        // GetDaemonConfig payload reflects the new value.
        let json = rc.to_json();
        assert_eq!(
            json.get("system_prompt_preset").and_then(|v| v.as_str()),
            Some(slug)
        );
    }
}

/// Label-aliased input is normalized to canonical slug and persisted as such.
#[test]
fn label_alias_input_persists_as_canonical_slug() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let store = Store::open(&db_path).unwrap();
    let state_path = db_dir.path().join("state.json");
    let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
    let config = Config::from_env();
    let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed);

    let stored =
        apply_update_via_handler_path(&rc, &store, "system_prompt_preset", "Code Only".into())
            .expect("apply update");
    assert_eq!(stored.as_str(), Some("code-only"));
    let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
    assert_eq!(row.as_deref(), Some("code-only"));
}

/// Non-system-prompt daemon settings now share the same durable path. This
/// simulates a daemon rebuild by dropping the first RuntimeConfig and applying
/// SQLite-backed rows to a fresh one.
#[test]
fn daemon_config_update_then_restart_returns_persisted_non_prompt_value() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let state_path = db_dir.path().join("state.json");

    {
        let store = Store::open(&db_path).unwrap();
        let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
        let config = Config::from_env();
        let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed);

        let stored = apply_update_via_handler_path(
            &rc,
            &store,
            "codex_sandbox_mode",
            "danger-full-access".into(),
        )
        .expect("apply update");
        assert_eq!(stored.as_str(), Some("danger-full-access"));
        assert_eq!(
            store
                .get_daemon_setting("codex_sandbox_mode")
                .unwrap()
                .as_deref(),
            Some("danger-full-access")
        );
    }

    {
        let store = Store::open(&db_path).unwrap();
        let seed = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
        let config = Config::from_env();
        let rc = RuntimeConfig::from_config_with_system_prompt_preset(&config, seed);
        apply_persisted_runtime_config(&store, &rc).expect("apply persisted config");

        assert_eq!(
            rc.to_json()
                .get("codex_sandbox_mode")
                .and_then(|v| v.as_str()),
            Some("danger-full-access")
        );
    }
}
