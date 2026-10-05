//! The boundary around the store crates (issue #1021 S4).
//!
//! Before the split, authority-bearing constructors (model-execution
//! capabilities, controller grants, the internal transfer actor) were
//! `pub(crate)` in `rsid`, so only daemon code could mint them. Rust has no
//! friend-crate visibility, so after the move those APIs are `pub` in
//! `rsid-store`, and the boundary is the dependency graph: `rsid-store` is
//! unpublished and `rsid` is its only dependent, `rsid` re-exports the
//! authority items crate-private, and the test seam (fakes that mint
//! capabilities) reaches a build only through dev-dependency edges. These
//! checks fail the build when a new crate or feature edge would widen that.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use toml::{Table, Value};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn manifests() -> BTreeMap<String, Table> {
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(crates_dir()).expect("crates directory") {
        let path = entry.expect("crate entry").path().join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&path) {
            let name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .expect("crate directory name")
                .to_owned();
            found.insert(name, text.parse::<Table>().expect("valid manifest"));
        }
    }
    found
}

/// `(section, table)` for every dependency table of a manifest, including the
/// target-specific ones. `section` is `dependencies`, `dev-dependencies` or
/// `build-dependencies`.
fn dependency_tables(manifest: &Table) -> Vec<(&'static str, &Table)> {
    let mut tables = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(Value::Table(table)) = manifest.get(section) {
            tables.push((section, table));
        }
        if let Some(Value::Table(targets)) = manifest.get("target") {
            for target in targets.values() {
                if let Some(Value::Table(table)) = target.get(section) {
                    tables.push((section, table));
                }
            }
        }
    }
    tables
}

fn dependents_of(dependency: &str) -> BTreeSet<String> {
    manifests()
        .iter()
        .filter(|(name, _)| name.as_str() != dependency)
        .filter(|(_, manifest)| {
            dependency_tables(manifest)
                .iter()
                .any(|(_, table)| table.contains_key(dependency))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn feature_list(entry: &Value) -> Vec<String> {
    entry
        .get("features")
        .and_then(Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(|feature| feature.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn only_the_daemon_crate_depends_on_the_store_crate() {
    assert_eq!(
        dependents_of("rsid-store"),
        BTreeSet::from(["rsid".to_owned()]),
        "rsid-store exposes daemon-only authority as `pub`; a second dependent would reach it. \
         Another crate that needs store data must go through the daemon's RPC instead."
    );
}

#[test]
fn only_the_store_crates_depend_on_the_core_crate() {
    assert_eq!(
        dependents_of("rsid-core"),
        BTreeSet::from(["rsid".to_owned(), "rsid-store".to_owned()])
    );
}

#[test]
fn the_store_crates_are_never_published() {
    let manifests = manifests();
    for name in ["rsid-store", "rsid-core"] {
        let manifest = manifests.get(name).expect("manifest");
        assert_eq!(
            manifest["package"].get("publish").and_then(Value::as_bool),
            Some(false),
            "{name} must stay `publish = false`"
        );
    }
}

#[test]
fn the_test_seam_reaches_a_build_only_through_dev_dependency_edges() {
    let manifests = manifests();
    for (crate_name, manifest) in &manifests {
        for (section, table) in dependency_tables(manifest) {
            if section == "dev-dependencies" {
                continue;
            }
            for dependency in ["rsid", "rsid-store", "rsid-core"] {
                if let Some(entry) = table.get(dependency) {
                    assert!(
                        !feature_list(entry)
                            .iter()
                            .any(|feature| feature == "test-seam"),
                        "{crate_name} enables test-seam on {dependency} in [{section}]; \
                         the seam (fake capabilities, route-validation bypasses) must only \
                         be reachable through a dev-dependency"
                    );
                }
            }
        }
    }
    // The seam features themselves: `default` never enables one, and each crate
    // forwards only from its own `test-seam` feature.
    for name in ["rsid", "rsid-store", "rsid-core"] {
        let features = manifests[name]
            .get("features")
            .and_then(Value::as_table)
            .expect("features table");
        for (feature, enabled) in features {
            let enabled: Vec<&str> = enabled
                .as_array()
                .map(|values| values.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if feature == "default" {
                assert!(
                    enabled.is_empty(),
                    "{name}: default features must stay empty"
                );
            } else if feature != "test-seam" {
                assert!(
                    !enabled.iter().any(|value| value.ends_with("test-seam")),
                    "{name}: feature {feature} must not enable a test seam"
                );
            }
        }
    }
    // Positive: the seam is available to the crates' own tests.
    for name in ["rsid", "rsid-store"] {
        let dev = manifests[name]
            .get("dev-dependencies")
            .and_then(Value::as_table)
            .expect("dev-dependencies");
        assert!(
            feature_list(&dev[name])
                .iter()
                .any(|feature| feature == "test-seam"),
            "{name} must enable its own test seam through its self dev-dependency"
        );
    }
}
