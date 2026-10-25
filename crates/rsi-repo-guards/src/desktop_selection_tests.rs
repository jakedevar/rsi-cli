//! Selector-only parity; the remaining desktop runner/private-tree/timeout
//! tests stay explicitly unported until their complete execution path exists.
use super::*;
use serde_json::Value as Json;

type Files = BTreeMap<String, String>;
fn fixture() -> Json {
    serde_json::from_str(include_str!(
        "../../../scripts/tests/fixtures/desktop-selection.json"
    ))
    .unwrap()
}
fn files() -> Files {
    let mut files: Files = fixture()["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect();
    // Presence marker only: no fixture writes or executes a Python source file.
    files.insert(SCRIPT.into(), String::new());
    files
}
fn select(files: &Files, path: &str) -> Result<bool, String> {
    lane_selected_with(|p| files.get(p).cloned(), &[path.into()])
}

#[test]
fn desktop_paths_and_static_inputs_select_the_lane() {
    let files = files();
    for path in [
        "desktop/src/model.ts",
        "desktop/package.json",
        "desktop/src-tauri/src/lib.rs",
        "Cargo.toml",
        "docs/keybindings.md",
        "crates/rsi/src/keybindings.rs",
        "desktop\\src\\model.ts",
    ] {
        assert_eq!(select(&files, path), Ok(true), "{path}");
    }
    assert!(lane_selected_with(|p| files.get(p).cloned(), &[]).is_ok_and(|s| !s));
}

#[test]
fn unrelated_paths_select_nothing() {
    let files = files();
    for path in [
        "crates/rsid/src/lib.rs",
        "crates/unrelated/src/lib.rs",
        "scripts/scoped-test",
        "docs/desktop/x.md",
        "docs/other.md",
        "desktopish/x",
        "desktop-notes.md",
        "crates/rsi/Cargo.toml",
    ] {
        assert_eq!(select(&files, path), Ok(false), "{path}");
    }
}

#[test]
fn shared_selection_cases_preserve_every_original_decision() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 31);
    for case in cases {
        let mut files: Files = fixture["files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        for (name, value) in case["overrides"].as_object().unwrap() {
            if let Some(text) = value.as_str() {
                files.insert(name.clone(), text.into());
            } else {
                files.remove(name);
            }
        }
        if case["selector"] == "present" {
            files.insert(SCRIPT.into(), String::new());
        }
        let path = case["path"].as_str().unwrap();
        if let Some(text) = case["contents"].as_str() {
            files.insert(path.into(), text.into());
        } else {
            files.remove(path);
        }
        if case["selector"] == "rename" {
            files.remove(SCRIPT);
        }
        let result = select(&files, path);
        match case["expected"].as_str().unwrap() {
            "refused" => assert!(
                result.is_err_and(|e| e.contains(SCRIPT)),
                "{}",
                case["name"]
            ),
            "selected" => assert_eq!(result, Ok(true), "{}", case["name"]),
            "empty" => assert_eq!(result, Ok(false), "{}", case["name"]),
            expected => panic!("unrecognized expected decision {expected}"),
        }
    }
}

#[test]
fn trigger_inputs_follow_direct_transitive_target_build_dev_and_workspace_edges() {
    let files = files();
    let inputs = trigger_inputs(|p| files.get(p).cloned());
    assert_eq!(inputs.first().unwrap(), "desktop");
    for input in [
        "Cargo.toml",
        "docs/keybindings.md",
        "crates/rsi/src",
        "crates/direct",
        "crates/deep",
        "crates/shared",
        "crates/kit",
        "crates/build",
        "crates/win",
    ] {
        assert!(inputs.contains(&input.into()), "{input}");
        assert_eq!(select(&files, &format!("{input}/src/file.rs")), Ok(true));
    }
}

#[test]
fn new_path_dependency_needs_no_selector_edit() {
    let mut files = files();
    files
        .get_mut(MANIFEST)
        .unwrap()
        .push_str("\n[dependencies.newcomer]\npath = '../../crates/unrelated'\n");
    assert_eq!(select(&files, "crates/unrelated/src/lib.rs"), Ok(true));
}

#[test]
fn outside_or_desktop_internal_dependencies_are_not_outside_inputs() {
    let mut files = files();
    files.get_mut(MANIFEST).unwrap().push_str("\n[dependencies.outside]\npath = '../../../elsewhere'\n[dependencies.inner]\npath = '../inner'\n");
    files.insert(
        "desktop/inner/Cargo.toml".into(),
        "[dependencies]\ndeep = {path = '../../crates/unrelated'}\n".into(),
    );
    let inputs = outside_inputs(|p| files.get(p).cloned());
    assert!(
        inputs
            .iter()
            .all(|s| !s.contains("elsewhere") && !s.starts_with("desktop") && !s.contains(".."))
    );
    // An internal dependency can itself reach an external dependency.
    assert!(inputs.contains(&"crates/unrelated".into()));
}

#[test]
fn missing_or_unparsable_manifests_keep_static_inputs() {
    let mut expected: Vec<_> = OUTSIDE_INPUTS.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    for text in [
        None,
        Some("not = [toml"),
        Some("[dependencies]\ntauri = '2'\n"),
    ] {
        assert_eq!(outside_inputs(|_| text.map(str::to_owned)), expected);
    }
}

#[test]
fn missing_selector_fails_closed_on_static_or_dependency_changes() {
    let mut files = files();
    files.remove(SCRIPT);
    for path in [
        SCRIPT,
        "desktop/src/app.ts",
        "docs/keybindings.md",
        "Cargo.toml",
        "crates/deep/src/lib.rs",
        "crates/shared/src/lib.rs",
    ] {
        let error = select(&files, path).unwrap_err();
        assert!(error.contains(&format!("{SCRIPT} is missing")), "{error}");
        assert!(error.contains(path), "{error}");
    }
    assert_eq!(select(&files, "docs/other.md"), Ok(false));
    assert_eq!(
        lane_selected_with(|p| files.get(p).cloned(), &[]),
        Ok(false)
    );
}

#[test]
fn strict_missing_selector_closure_refuses_unknown_edges() {
    for text in [
        "not = [toml",
        "[dependencies]\nbad = 123",
        "[dependencies]\nbad = {path = '../../../outside'}",
        "[dependencies]\nbad = {path = 'C:\\outside'}",
        "[dependencies]\nbad = {path = 2}",
        "[dependencies]\nbad = {workspace = true}",
        "[dependencies]\nbad = {path = '../../crates/missing'}",
        "dependencies = 1",
        "workspace = 1",
        "target = 1",
        "[target]\na = 1",
        "patch = 1",
        "[patch]\na = 1",
        "replace = 1",
    ] {
        let mut files = files();
        files.remove(SCRIPT);
        files.insert(MANIFEST.into(), text.into());
        let error = select(&files, "docs/other.md").unwrap_err();
        assert!(
            error.contains("cannot establish desktop dependency closure"),
            "{text}: {error}"
        );
        // A static input refusal is available even when manifests are broken.
        assert!(
            select(&files, "desktop/src/app.ts")
                .unwrap_err()
                .contains("diff touches")
        );
    }
}

#[test]
fn inherited_dependencies_use_nearest_or_explicit_workspace_when_selector_missing() {
    let mut files = files();
    files.remove(SCRIPT);
    files.insert("crates/direct/Cargo.toml".into(), "[package]\nname='direct'\nworkspace='../local-workspace'\n[dependencies]\nlocal.workspace=true\n".into());
    files.insert(
        "crates/local-workspace/Cargo.toml".into(),
        "[workspace.dependencies]\nlocal={path='../unrelated'}\n".into(),
    );
    assert!(
        missing_selector_inputs(|p| files.get(p).cloned())
            .unwrap()
            .contains(&"crates/unrelated".into())
    );
    assert!(select(&files, "crates/unrelated/src/lib.rs").is_err());
    files
        .get_mut("crates/direct/Cargo.toml")
        .unwrap()
        .push_str("missing.workspace=true\n");
    assert!(select(&files, "docs/other.md").is_err());
}

#[test]
fn strict_closure_follows_patch_replace_and_dependency_cycles() {
    let mut files = files();
    files.remove(SCRIPT);
    files.get_mut(MANIFEST).unwrap().push_str("\n[patch.crates-io]\nlocal={path='../../crates/unrelated'}\n[replace]\n'old:1.0.0'={path='../../crates/replaced'}\n");
    files.insert(
        "crates/replaced/Cargo.toml".into(),
        "[dependencies]\ncycle={path='../direct'}\n".into(),
    );
    let inputs = missing_selector_inputs(|p| files.get(p).cloned()).unwrap();
    assert!(inputs.contains(&"crates/unrelated".into()));
    assert!(inputs.contains(&"crates/replaced".into()));
    assert_eq!(select(&files, "docs/other.md"), Ok(false));
}

#[test]
fn missing_desktop_manifest_keeps_only_static_fallback() {
    let mut files = files();
    files.remove(SCRIPT);
    files.remove(MANIFEST);
    let inputs = missing_selector_inputs(|p| files.get(p).cloned()).unwrap();
    assert_eq!(
        inputs.into_iter().collect::<BTreeSet<_>>(),
        static_inputs().into_iter().collect()
    );
    assert_eq!(select(&files, "docs/other.md"), Ok(false));
}
