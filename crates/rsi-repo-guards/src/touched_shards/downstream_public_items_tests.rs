//! Port of all eight `test_downstream_public_items.py` cases (#366, #3408).
//! Each test retains its legacy name (without `test_`), forming the case map.
//! Real temporary Git commits drive the production Rust planner; the first two
//! cases also exercise its staged CLI JSON contract. No legacy program runs.
//! Canonical selector/runner cutover remains with #426/#423.
use super::{derive_tests::Case, downstream_tests::downstream_test_plan};
use crate::{Ctx, scoped_runner::selection::selection_plan};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const ITEM: &str = "AUTO_COMPACT_MESSAGE_TRIGGER";
const LIBRARY: &str = "crates/rsid-harness/src/compaction.rs";

fn source_for_tests(names: &[&str], reference: &str) -> String {
    let mut text = String::from("#[cfg(test)]\nmod tests {\n");
    for name in names {
        text.push_str(&format!(
            "    #[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-01\"))]\n    #[test]\n    fn {name}() {{\n        let _ = {reference};\n    }}\n"
        ));
    }
    text + "}\n"
}

fn case() -> Case {
    let mut case = Case::empty();
    case.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    for (name, dep) in [
        ("rsid-harness", None),
        ("bridge", Some("rsid-harness")),
        ("rsid", Some("bridge")),
        ("unrelated", None),
        ("rsid-store", None),
    ] {
        let mut manifest = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
        if let Some(dep) = dep {
            manifest.push_str(&format!(
                "[dependencies]\n{dep} = {{ path = \"../{dep}\" }}\n"
            ));
        }
        if name == "rsid" || name == "rsid-store" {
            let shard = if name == "rsid" {
                "session-01"
            } else {
                "store-01"
            };
            manifest.push_str(&format!(
                "[features]\ndefault = []\ntest-shard-mode = []\ntest-shard-{shard} = [\"test-shard-mode\"]\n"
            ));
        }
        case.write(&format!("crates/{name}/Cargo.toml"), &manifest);
    }
    // Establish an empty desktop dependency closure without a Python selector fixture.
    case.write(
        "desktop/src-tauri/Cargo.toml",
        "[package]\nname = \"desktop\"\nversion = \"0.1.0\"\n",
    );
    case.write("crates/rsid-harness/src/lib.rs", "pub mod compaction;\n");
    case.write(
        LIBRARY,
        &format!("pub const {ITEM}: usize = 150;\nfn private_value() -> usize {{\n    1\n}}\n"),
    );
    case.write(
        "crates/bridge/src/lib.rs",
        &format!("pub use rsid_harness::compaction::{ITEM};\n"),
    );
    case.write("crates/rsid/src/lib.rs", "mod session;\n");
    case.write("crates/rsid/src/session.rs", "mod harness;\n");
    case.write(
        "crates/rsid/src/session/harness.rs",
        "mod compaction;\nmod agent_loop;\nmod unrelated;\n",
    );
    case.write(
        "crates/rsid/src/session/harness/compaction.rs",
        &(format!("use bridge::{ITEM};\n")
            + &source_for_tests(&["persists_summary", "rebuilds_tail"], "1")),
    );
    case.write(
        "crates/rsid/src/session/harness/agent_loop.rs",
        &source_for_tests(&["continues_after_compaction"], &format!("bridge::{ITEM}")),
    );
    case.write(
        "crates/rsid/src/session/harness/unrelated.rs",
        &source_for_tests(&["unrelated"], "1"),
    );
    case.write(
        "crates/unrelated/src/lib.rs",
        &format!("const {ITEM}: usize = 0;\n#[test]\nfn same_name() {{}}\n"),
    );
    case.write("crates/rsid-store/src/lib.rs", "");
    case.commit();
    case.rebase_base_here();
    case
}

fn plan(case: &Case) -> Value {
    let loaded = case.load(&case.base, &case.head).unwrap();
    downstream_test_plan(&loaded.repo, &case.base, &case.head, &loaded.sources).unwrap()
}

fn change(case: &mut Case, before: &str, after: &str) -> Value {
    let text = case.read(LIBRARY);
    assert!(text.contains(before), "fixture edit must change {before:?}");
    case.rebase_base_here();
    case.write(LIBRARY, &text.replace(before, after));
    case.commit();
    plan(case)
}

fn strings(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn expected_filters() -> BTreeSet<String> {
    [
        "compaction::tests::persists_summary",
        "compaction::tests::rebuilds_tail",
        "agent_loop::tests::continues_after_compaction",
    ]
    .map(|name| format!("rsid=shard:session-01:test(=session::harness::{name})"))
    .into()
}

fn cli_receipt(case: &Case) -> Value {
    let ctx = Ctx::new(case.fx.path(), &[("RSI_TOUCHED_SHARDS_STAGED", "1")]);
    let args = [
        "--repo",
        case.fx.path().to_str().unwrap(),
        "--base",
        &case.base,
        "--head",
        &case.head,
        "--filters-only",
        "--json",
    ]
    .map(str::to_owned);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = super::main(&args, &ctx, &mut out, &mut err);
    assert_eq!(
        code,
        0,
        "{}\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn compaction_constant_selects_exact_rsids_tests_through_transitive_dependent() {
    let mut case = case();
    let plan = change(&mut case, "= 150", "= 180");
    let expected = expected_filters();
    assert_eq!(strings(&plan["filters"]), expected);
    assert_eq!(
        plan["items"],
        json!([{"package":"rsid-harness", "item":ITEM, "paths":[LIBRARY]}])
    );
    let selected: BTreeSet<_> = plan["selections"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| !s["tests"].as_array().unwrap().is_empty())
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        selected,
        BTreeSet::from([
            "crates/rsid/src/session/harness/compaction.rs",
            "crates/rsid/src/session/harness/agent_loop.rs"
        ])
    );
    let receipt = cli_receipt(&case);
    assert_eq!(strings(&receipt["downstream_tests"]["filters"]), expected);
    assert_eq!(strings(&receipt["worker_filters"]), expected);
    assert_eq!(receipt["unfiltered_leaf_paths"], json!([LIBRARY]));
}

#[test]
fn nonleaf_library_publishes_exact_observers_to_queue() {
    let mut case = case();
    let path = "crates/bridge/src/lib.rs";
    case.write(path, &format!("pub const {ITEM}: usize = 150;\n"));
    case.commit();
    case.rebase_base_here();
    case.write(path, &format!("pub const {ITEM}: usize = 180;\n"));
    case.commit();
    let receipt = cli_receipt(&case);
    assert_eq!(strings(&receipt["suggested_filters"]), expected_filters());
    assert_eq!(
        strings(&receipt["downstream_tests"]["filters"]),
        expected_filters()
    );
}

#[test]
fn private_comment_and_test_only_edits_select_no_extra_tests() {
    let mut case = case();
    for (before, after) in [("    1", "    2"), ("= 150;", "= 150; // document trigger")] {
        assert_eq!(change(&mut case, before, after)["filters"], json!([]));
    }
    case.write(LIBRARY, &(case.read(LIBRARY) + "#[cfg(test)]\nmod tests {\n    pub fn test_helper() -> usize {\n        1\n    }\n}\n"));
    case.commit();
    assert_eq!(
        change(&mut case, "        1", "        2")["filters"],
        json!([])
    );
}

#[test]
fn public_comments_beside_private_code_and_test_mounts_add_no_observers() {
    let mut case = case();
    case.write(
        LIBRARY,
        &case.read(LIBRARY).replace("= 150;", "= 150; // before"),
    );
    case.commit();
    case.rebase_base_here();
    case.write(
        LIBRARY,
        &case
            .read(LIBRARY)
            .replace("// before", "// after")
            .replace("    1", "    2"),
    );
    case.commit();
    let result = plan(&case);
    assert_eq!(result["items"], json!([]));
    assert_eq!(result["filters"], json!([]));
    let path = "crates/rsid-harness/src/tests.rs";
    case.write(
        "crates/rsid-harness/src/lib.rs",
        "pub mod compaction;\n#[cfg(test)]\nmod tests;\n",
    );
    case.write(path, &format!("pub const {ITEM}: usize = 150;\n"));
    case.commit();
    case.rebase_base_here();
    case.write(path, &format!("pub const {ITEM}: usize = 180;\n"));
    case.commit();
    assert_eq!(plan(&case)["filters"], json!([]));
}

#[test]
fn generic_function_names_are_skipped_with_receipt_note() {
    let mut case = case();
    for name in ["new", "default", "from"] {
        case.write(
            LIBRARY,
            &format!("pub fn {name}() -> usize {{\n    1\n}}\n"),
        );
        case.commit();
        let result = change(&mut case, "    1", "    2");
        assert_eq!(result["filters"], json!([]));
        assert_eq!(
            result["notes"],
            json!([{"package":"rsid-harness", "item":name, "reason":"generic identifier; compile coverage retained"}])
        );
    }
}

#[test]
fn unsharded_targets_and_store_keep_runnable_exact_selectors() {
    let mut case = case();
    case.write(
        "crates/bridge/src/lib.rs",
        &format!(
            "pub use rsid_harness::compaction::{ITEM};\n#[test]\nfn observes_trigger() {{}}\n"
        ),
    );
    case.write(
        "crates/bridge/src/bin/tool.rs",
        &format!("use bridge::{ITEM};\nfn main() {{}}\n#[test]\nfn observes_cli() {{}}\n"),
    );
    case.write("crates/rsid-store/Cargo.toml", "[package]\nname = \"rsid-store\"\nversion = \"0.1.0\"\n[dependencies]\nrsid-harness = { path = \"../rsid-harness\" }\n");
    case.write("crates/rsid-store/src/lib.rs", &format!("use rsid_harness::compaction::{ITEM};\n#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\"))]\n#[test]\nfn observes_store() {{}}\n"));
    case.commit();
    let result = change(&mut case, "= 150", "= 180");
    let expected: BTreeSet<String> = [
        "bridge=test(=observes_trigger)",
        "bridge=bin:tool:test(=observes_cli)",
        "rsid=shard:store-01:test(=observes_store)",
    ]
    .map(str::to_owned)
    .into();
    assert!(expected.is_subset(&strings(&result["filters"])), "{result}");
    let selected = selection_plan(
        &expected.into_iter().collect::<Vec<_>>(),
        &BTreeMap::from([("store-01".into(), vec!["rsid-store".into()])]),
        &[],
    )
    .unwrap();
    let routed: BTreeMap<_, BTreeSet<_>> = selected
        .iter()
        .map(|(p, tests)| {
            (
                p.as_str(),
                tests.iter().map(|s| s.exact.as_deref().unwrap()).collect(),
            )
        })
        .collect();
    assert_eq!(
        routed,
        BTreeMap::from([
            (
                "bridge",
                BTreeSet::from(["observes_trigger", "observes_cli"])
            ),
            ("rsid-store", BTreeSet::from(["observes_store"]))
        ])
    );
    let cli = selected["bridge"]
        .iter()
        .find(|s| s.exact.as_deref() == Some("observes_cli"))
        .unwrap();
    assert_eq!((&*cli.kind, &*cli.target), ("bin", "tool"));
}

#[test]
fn high_fan_in_identifier_is_capped_with_receipt_note() {
    let mut case = case();
    let mut root = String::new();
    for index in 0..41 {
        let name = format!("consumer_{index}");
        root.push_str(&format!("mod {name};\n"));
        case.write(
            &format!("crates/rsid/src/{name}.rs"),
            &(format!("use bridge::{ITEM};\n") + &source_for_tests(&["observes"], "1")),
        );
    }
    case.write("crates/rsid/src/lib.rs", &root);
    case.commit();
    let result = change(&mut case, "= 150", "= 180");
    assert_eq!(result["filters"], json!([]));
    assert_eq!(result["notes"][0]["cap"], 40);
    assert!(
        result["notes"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("reference file cap exceeded")
    );
}

#[test]
fn public_definitions_include_body_type_and_removal_changes() {
    let mut case = case();
    for (before, after, name) in [
        (
            "pub fn trigger_count() -> u8 {\n    1\n}\n",
            "pub fn trigger_count() -> u8 {\n    2\n}\n",
            "trigger_count",
        ),
        (
            "pub struct Record {\n    value: u8,\n}\n",
            "pub struct Record {\n    value: u16,\n}\n",
            "Record",
        ),
        (
            "pub enum Choice { First }\n",
            "pub enum Choice { Second }\n",
            "Choice",
        ),
        (
            "pub type TriggerCount = u8;\n",
            "pub type TriggerCount = u16;\n",
            "TriggerCount",
        ),
        (
            "pub static TRIGGER_COUNT: u8 = 1;\n",
            "pub static TRIGGER_COUNT: u8 = 2;\n",
            "TRIGGER_COUNT",
        ),
        (
            "pub const AUTO_COMPACT_MESSAGE_TRIGGER: usize = 150;\n",
            "",
            ITEM,
        ),
    ] {
        case.write(LIBRARY, before);
        case.commit();
        let result = change(&mut case, before, after);
        let names: Vec<_> = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["item"].as_str().unwrap())
            .collect();
        assert_eq!(names, [name]);
    }
}
