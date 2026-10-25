//! Real-checkout residue of scripts/tests/test_desktop_test_gate.py (#487).
//!
//! Current legacy CommittedManifest case map (all three surviving methods):
//! - test_the_committed_trigger_set_is_the_desktop_root_manifest_the_lock_crates_and_the_outside_inputs
//!   -> real_committed_trigger_set_matches_lock_and_filesystem_reader.
//! - test_the_lock_names_no_local_package_so_no_crate_directory_is_missing_from_the_inputs
//!   -> real_lock_crates_have_manifests_and_are_exactly_the_crate_inputs.
//! - test_the_committed_set_selects_the_inputs_and_not_crates_the_app_does_not_consume
//!   -> real_inputs_select_consumed_paths_and_keep_deliberate_crate_anchors.
//!
//! The legacy subject still derives inputs from manifests; the native selector
//! does too. Its existing JSON fixture tests do not cover this real lockfile.
//! Expectations here come independently from source-less lock packages and
//! literal outside inputs, never from the selector's dependency traversal or
//! OUTSIDE_INPUTS. Negative cases use only in-memory TOML/maps/input vectors.
//! No Cargo, browser, runner, process cleanup or caller cutover is exercised;
//! the only subprocess reads are Git reads of the committed checkout.
use super::{MANIFEST, selected, trigger_inputs};
use crate::Ctx;
use crate::released_migrations::git_show_opt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use toml::Value;

const LOCK: &str = "desktop/src-tauri/Cargo.lock";
type Files = BTreeMap<String, String>;

fn repo_root() -> PathBuf {
    Path::new(rsi_common::manifest_dir!())
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn fs_read(root: &Path, path: &str) -> Option<String> {
    match std::fs::read_to_string(root.join(path)) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => panic!("cannot read {path}: {error}"),
    }
}

fn local_lock_crates(
    mut read: impl FnMut(&str) -> Option<String>,
) -> Result<BTreeSet<String>, String> {
    let text = read(LOCK).ok_or_else(|| format!("cannot read {LOCK}"))?;
    let lock: Value = text.parse().map_err(|e| format!("{LOCK}: {e}"))?;
    let packages = lock
        .get("package")
        .and_then(Value::as_array)
        .ok_or("lock has no package array")?;
    let mut directories = BTreeSet::new();
    for package in packages {
        if package.get("source").is_some() {
            continue;
        }
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or("local lock package has no name")?;
        if name == "rsi-desktop" {
            continue;
        }
        let directory = format!("crates/{name}");
        let path = format!("{directory}/Cargo.toml");
        let text =
            read(&path).ok_or_else(|| format!("local lock package missing manifest: {path}"))?;
        let manifest: Value = text.parse().map_err(|e| format!("{path}: {e}"))?;
        if manifest
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            != Some(name)
        {
            return Err(format!("{path} does not declare lock package {name}"));
        }
        directories.insert(directory);
    }
    if directories.is_empty() {
        return Err("desktop lock must consume repository crates".into());
    }
    Ok(directories)
}

fn expected_inputs(local_crates: &BTreeSet<String>) -> BTreeSet<String> {
    [
        "desktop",
        "Cargo.toml",
        "crates/rsi/src",
        "docs/keybindings.md",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(local_crates.iter().cloned())
    .collect()
}

fn check_inputs(inputs: &[String], local_crates: &BTreeSet<String>) -> Result<(), String> {
    let actual: BTreeSet<_> = inputs.iter().cloned().collect();
    let expected = expected_inputs(local_crates);
    if actual != expected {
        return Err(format!(
            "trigger inputs differ: missing {:?}, unexpected {:?}",
            expected.difference(&actual).collect::<Vec<_>>(),
            actual.difference(&expected).collect::<Vec<_>>()
        ));
    }
    if actual.len() != inputs.len() {
        return Err("duplicate trigger inputs".into());
    }
    Ok(())
}

#[test]
fn real_committed_trigger_set_matches_lock_and_filesystem_reader() {
    let root = repo_root();
    let fs_crates = local_lock_crates(|p| fs_read(&root, p)).unwrap();
    let fs_inputs = trigger_inputs(|p| fs_read(&root, p));
    check_inputs(&fs_inputs, &fs_crates).unwrap();

    let ctx = Ctx::new(&root, &[]);
    // The shared reader distinguishes a missing blob from an unreadable
    // revision/object; a failed Git read cannot silently shrink the inventory.
    let committed_crates = local_lock_crates(|p| git_show_opt(&ctx, "HEAD", p).unwrap()).unwrap();
    let committed_inputs = trigger_inputs(|p| git_show_opt(&ctx, "HEAD", p).unwrap());
    check_inputs(&committed_inputs, &committed_crates).unwrap();
    assert_eq!(committed_crates, fs_crates);
    assert_eq!(committed_inputs, fs_inputs);
}

#[test]
fn real_lock_crates_have_manifests_and_are_exactly_the_crate_inputs() {
    let root = repo_root();
    let directories = local_lock_crates(|p| fs_read(&root, p)).unwrap();
    assert!(!directories.is_empty());
    let inputs = trigger_inputs(|p| fs_read(&root, p));
    for directory in &directories {
        assert!(
            root.join(directory).join("Cargo.toml").is_file(),
            "{directory}"
        );
        assert!(inputs.contains(directory), "{directory}");
    }
    let crate_inputs: BTreeSet<_> = inputs
        .into_iter()
        .filter(|p| p.starts_with("crates/") && p != "crates/rsi/src")
        .collect();
    assert_eq!(crate_inputs, directories);
}

#[test]
fn real_inputs_select_consumed_paths_and_keep_deliberate_crate_anchors() {
    let root = repo_root();
    let directories = local_lock_crates(|p| fs_read(&root, p)).unwrap();
    let inputs = trigger_inputs(|p| fs_read(&root, p));
    for path in [
        "Cargo.toml",
        "desktop/src/model.ts",
        "docs/keybindings.md",
        "crates/rsi/src/ui/mod.rs",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(directories.iter().map(|d| format!("{d}/src/lib.rs")))
    {
        assert!(selected(&[path.clone()], &inputs), "{path}");
    }
    for path in ["crates/rsid/src/lib.rs", "README.md"] {
        assert_eq!(selected(&[path.into()], &inputs), false, "{path}");
    }
    // These current dependencies must be dropped deliberately, even if the
    // lockfile and manifests are changed together to agree on a smaller set.
    for directory in ["crates/rsi-common", "crates/rsi-graph"] {
        assert!(directories.contains(directory), "{directory}");
    }
}

fn fixture() -> Files {
    BTreeMap::from([
        (LOCK.into(), "[[package]]\nname = 'rsi-desktop'\n[[package]]\nname = 'local'\n[[package]]\nname = 'external'\nsource = 'registry+https://example.invalid/index'\n".into()),
        (MANIFEST.into(), "[package]\nname = 'rsi-desktop'\n[workspace]\n[dependencies]\nlocal = { path = '../../crates/local' }\n".into()),
        ("crates/local/Cargo.toml".into(), "[package]\nname = 'local'\n".into()),
    ])
}

#[test]
fn lock_contract_detects_a_manifest_edge_that_omits_a_required_crate() {
    let mut files = fixture();
    let crates = local_lock_crates(|p| files.get(p).cloned()).unwrap();
    assert_eq!(crates, BTreeSet::from(["crates/local".into()]));
    check_inputs(&trigger_inputs(|p| files.get(p).cloned()), &crates).unwrap();
    files.insert(
        MANIFEST.into(),
        "[package]\nname = 'rsi-desktop'\n[workspace]\n".into(),
    );
    let inputs = trigger_inputs(|p| files.get(p).cloned());
    assert!(
        check_inputs(&inputs, &crates)
            .unwrap_err()
            .contains("crates/local")
    );
    assert_eq!(
        selected(&["crates/local/src/lib.rs".into()], &inputs),
        false
    );
}

#[test]
fn lock_contract_detects_a_source_less_package_without_its_manifest() {
    let mut files = fixture();
    local_lock_crates(|p| files.get(p).cloned()).unwrap();
    files
        .get_mut(LOCK)
        .unwrap()
        .push_str("\n[[package]]\nname = 'missing'\n");
    assert_eq!(
        local_lock_crates(|p| files.get(p).cloned()),
        Err("local lock package missing manifest: crates/missing/Cargo.toml".into())
    );
}

#[test]
fn lock_contract_requires_a_nonempty_local_inventory() {
    let mut files = fixture();
    files.insert(LOCK.into(), "[[package]]\nname = 'rsi-desktop'\n[[package]]\nname = 'external'\nsource = 'registry+https://example.invalid/index'\n".into());
    assert_eq!(
        local_lock_crates(|p| files.get(p).cloned()),
        Err("desktop lock must consume repository crates".into())
    );
}

#[test]
fn trigger_contract_detects_each_removed_required_input() {
    let files = fixture();
    let crates = local_lock_crates(|p| files.get(p).cloned()).unwrap();
    let inputs = trigger_inputs(|p| files.get(p).cloned());
    check_inputs(&inputs, &crates).unwrap();
    for required in expected_inputs(&crates) {
        let mut incomplete = inputs.clone();
        incomplete.retain(|p| p != &required);
        assert!(
            check_inputs(&incomplete, &crates)
                .unwrap_err()
                .contains(&required),
            "{required}"
        );
    }
}

#[test]
fn trigger_contract_detects_an_extra_crate_input() {
    let files = fixture();
    let crates = local_lock_crates(|p| files.get(p).cloned()).unwrap();
    let mut inputs = trigger_inputs(|p| files.get(p).cloned());
    check_inputs(&inputs, &crates).unwrap();
    inputs.push("crates/unconsumed".into());
    assert!(
        check_inputs(&inputs, &crates)
            .unwrap_err()
            .contains("crates/unconsumed")
    );
}
