//! hub #3039: the one-time import of the TUI's legacy custom providers. The
//! state file lives in a temporary directory; keys are obviously fake.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::tests::{FAKE_KEY, Fixture};
use super::{custom_providers, legacy_import};
use rsi_common::operator_config::{CustomProviderKeyState, METHOD_DELETE_CUSTOM_PROVIDER};
use std::fs;
use uuid::Uuid;

fn legacy_state_json(ids: [Uuid; 3]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "selected_index": 3,
        "settings": {
            "submit_on_enter": true,
            "custom_providers": [
                {"id": ids[0], "name": "Groq", "base_url": "https://api.example.invalid/v1",
                 "api_key": FAKE_KEY, "default_model": "llama-test"},
                {"id": ids[1], "name": "Ollama", "base_url": "http://127.0.0.1:11434/v1",
                 "api_key": "", "default_model": ""},
                {"id": ids[2], "name": "", "base_url": "", "api_key": "", "default_model": ""},
                {"not": "an entry"}
            ]
        }
    }))
    .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn legacy_import_is_one_time_idempotent_and_leaves_state_json_byte_identical() {
    let fixture = Fixture::new();
    let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let state = &fixture.paths.legacy_tui_state;
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    fs::write(state, legacy_state_json(ids)).unwrap();
    let before = fs::read(state).unwrap();
    let before_modified = fs::metadata(state).unwrap().modified().unwrap();

    let report =
        legacy_import::import_legacy_tui_state(&fixture.store, &fixture.vault, &fixture.paths)
            .unwrap();
    assert!(report.source_present);
    assert_eq!(report.providers_imported, 2);
    assert_eq!(report.keys_imported, 1);
    assert_eq!(report.providers_skipped, 2);

    let list = custom_providers::list(&fixture.store, &fixture.vault).unwrap();
    let names: Vec<_> = list.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Groq", "Ollama"]);
    assert_eq!(list.providers[0].id, ids[0]);
    assert_eq!(list.providers[0].api_key.state, CustomProviderKeyState::Set);
    assert_eq!(
        list.providers[1].api_key.state,
        CustomProviderKeyState::Absent
    );
    assert_eq!(
        fixture.vault.custom_provider_key(ids[0]).unwrap().expose(),
        FAKE_KEY
    );

    // Second run: the marker stops it.
    let again =
        legacy_import::import_legacy_tui_state(&fixture.store, &fixture.vault, &fixture.paths)
            .unwrap();
    assert!(again.already_imported);

    // Crash before the marker: a rerun converges without duplicates and does
    // not resurrect a provider the operator deleted meanwhile.
    fixture.ok(
        METHOD_DELETE_CUSTOM_PROVIDER,
        serde_json::json!({"id": ids[1]}),
    );
    let rerun =
        legacy_import::import_entries(&fixture.store, &fixture.vault, &fixture.paths).unwrap();
    assert_eq!(rerun.providers_imported, 0);
    assert_eq!(rerun.keys_imported, 0);
    let names: Vec<_> = custom_providers::list(&fixture.store, &fixture.vault)
        .unwrap()
        .providers
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["Groq"]);

    // The operator's file is untouched: same bytes, same modification time.
    assert_eq!(fs::read(state).unwrap(), before);
    assert_eq!(
        fs::metadata(state).unwrap().modified().unwrap(),
        before_modified
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
#[test]
fn legacy_import_without_a_state_file_marks_done_and_writes_nothing_else() {
    let fixture = Fixture::new();
    let report =
        legacy_import::import_legacy_tui_state(&fixture.store, &fixture.vault, &fixture.paths)
            .unwrap();
    assert!(!report.source_present);
    assert_eq!(report.providers_imported, 0);
    assert!(!fixture.paths.legacy_tui_state.exists());
    assert!(
        fixture
            .store
            .get_daemon_setting(legacy_import::LEGACY_IMPORT_MARKER)
            .unwrap()
            .is_some()
    );
}
