//! RSI-026 — backcompat import test for `maybe_import_legacy_system_prompt_preset`.
//!
//! Phase 4 — cross-layer integration tests. The lib-level unit tests in
//! `crates/rsid/src/store/daemon_settings.rs` already cover the happy paths;
//! this file locks the end-to-end import-then-restart guarantees that the
//! `master_implement` verification manifest demands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rsid::store::Store;
use rsid::store::daemon_settings::{
    KEY_SYSTEM_PROMPT_PRESET, maybe_import_legacy_system_prompt_preset,
};
use tempfile::TempDir;

/// End-to-end legacy import:
/// 1. Pre-create a state.json at a temp path with `"system_prompt_preset":"Concise"`.
/// 2. Open a fresh Store at a temp DB.
/// 3. Call `maybe_import_legacy_system_prompt_preset`.
/// 4. Assert the row is inserted with value `"concise"`.
/// 5. Mutate state.json to set preset to `"Caveman"`.
/// 6. Call import again. Assert the row value is still `"concise"` (idempotent —
///    the second call MUST NOT re-read state.json once the row exists).
/// 7. Open a different fresh Store at a different temp DB with a NONEXISTENT
///    state.json path. Assert the row is inserted with value `"default"`
///    (clean-install fallback).
#[test]
fn import_then_restart_preserves_imported_value() {
    // Step 1-4 — first import.
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let state_path = db_dir.path().join("state.json");

    std::fs::write(
        &state_path,
        r#"{"settings":{"system_prompt_preset":"Concise"}}"#,
    )
    .unwrap();

    let store = Store::open(&db_path).unwrap();
    let first =
        maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("first import");
    assert_eq!(first, "concise");
    let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
    assert_eq!(row.as_deref(), Some("concise"));

    // Step 5 — mutate state.json.
    std::fs::write(
        &state_path,
        r#"{"settings":{"system_prompt_preset":"Caveman"}}"#,
    )
    .unwrap();

    // Step 6 — idempotent. The store already has the row; state.json must
    // be ignored on this call.
    let second =
        maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("second import");
    assert_eq!(
        second, "concise",
        "second import must return the existing row, not re-read state.json"
    );

    // Step 7 — clean install with no state.json.
    let other_db_dir = TempDir::new().unwrap();
    let other_db_path = other_db_dir.path().join("rsi2.db");
    let nonexistent_state = other_db_dir.path().join("does-not-exist.json");
    let other_store = Store::open(&other_db_path).unwrap();
    let clean = maybe_import_legacy_system_prompt_preset(&other_store, &nonexistent_state)
        .expect("clean install import");
    assert_eq!(clean, "default");
    let row = other_store
        .get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET)
        .unwrap();
    assert_eq!(row.as_deref(), Some("default"));
}

/// Re-opening the same Store after an import returns the SAME value via
/// `get_daemon_setting` and via re-invoking the import helper. This simulates
/// the daemon restart path.
#[test]
fn restart_preserves_imported_value() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let state_path = db_dir.path().join("state.json");

    std::fs::write(
        &state_path,
        r#"{"settings":{"system_prompt_preset":"Caveman"}}"#,
    )
    .unwrap();

    // Simulated daemon boot 1: import.
    {
        let store = Store::open(&db_path).unwrap();
        let v = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
        assert_eq!(v, "caveman");
    } // store drops, simulating daemon exit.

    // Wipe state.json — proves the second boot does NOT re-read it.
    std::fs::remove_file(&state_path).unwrap();

    // Simulated daemon boot 2: re-import should hit the idempotency early-return.
    {
        let store = Store::open(&db_path).unwrap();
        let v = maybe_import_legacy_system_prompt_preset(&store, &state_path).unwrap();
        assert_eq!(
            v, "caveman",
            "restart must surface the persisted value, not re-default"
        );
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some("caveman"));
    }
}

/// The full lower-case slug set is accepted on direct write too (set_daemon_setting).
/// This is the path the RpcServer takes after `update_field` succeeds.
#[test]
fn direct_set_daemon_setting_persists_each_canonical_slug() {
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("rsi.db");
    let store = Store::open(&db_path).unwrap();

    for slug in ["default", "concise", "code-only", "caveman"] {
        store
            .set_daemon_setting(KEY_SYSTEM_PROMPT_PRESET, slug)
            .expect("set_daemon_setting");
        let v = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(v.as_deref(), Some(slug));
    }
}
