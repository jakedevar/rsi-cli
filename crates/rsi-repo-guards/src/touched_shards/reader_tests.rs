use super::{identity::ItemReferences, readers, source::SourceTree};
use std::collections::{BTreeMap, BTreeSet};
const DATA: &str = "crates/rsid/src/session/guidance/w.md";
fn gate(name: &str, body: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-03\"))]\n#[test]\nfn {name}() {{\n    {body}\n}}\n"
    )
}
fn fixture(binding: &str, count: usize, extra: &str) -> SourceTree {
    let names = (0..count)
        .map(|n| gate(&format!("render_{n}"), "render();"))
        .collect::<String>();
    SourceTree::new(
        BTreeMap::from([
            ("crates/rsid/src/lib.rs".into(), "pub mod session;\n".into()),
            (
                "crates/rsid/src/session/mod.rs".into(),
                "mod preamble;\nmod other;\npub fn central() { preamble::render(); }\n".into(),
            ),
            (
                "crates/rsid/src/session/preamble.rs".into(),
                format!(
                    "{binding}\npub fn render() -> usize {{ TEXT.len() }}\n{names}{}",
                    gate("unrelated", "other();")
                ),
            ),
            (
                "crates/rsid/src/session/other.rs".into(),
                format!(
                    "{}{}{}{extra}",
                    gate("central_observer", "central();"),
                    gate("far", "central_observer();"),
                    gate("other", "unrelated();")
                ),
            ),
            (DATA.into(), "new guidance\n".into()),
        ]),
        "linux",
    )
}
fn included(sources: &SourceTree) -> Result<Vec<String>, String> {
    let index = ItemReferences::new(sources).unwrap();
    readers::include_item_tests(sources, &index, &BTreeSet::from([DATA.into()]))
        .map(|tests| tests.into_iter().map(|t| t.name).collect())
}
const BINDING: &str = "const TEXT: &str = include_str!(\"guidance/w.md\");";

#[test]
fn data_include_owners_follow_internal_closure_and_one_external_layer() {
    assert_eq!(
        included(&fixture(BINDING, 1, "")).unwrap(),
        ["central_observer", "render_0"]
    );
}

#[test]
fn data_include_binding_accepts_const_static_and_function_owners() {
    for binding in [
        BINDING,
        "static TEXT: &str = include_str!(\"guidance/w.md\");",
        "pub fn TEXT() -> &'static str {\n    include_str!(\"guidance/w.md\")\n}",
        "const TEXT:\n    &[u8]\n    = include_bytes!(\"guidance/w.md\");",
    ] {
        assert_eq!(
            included(&fixture(binding, 1, "")).unwrap(),
            ["central_observer", "render_0"],
            "{binding}"
        );
    }
}

#[test]
fn include_item_unknown_owner_another_literal_or_outside_reader_retains_fallback() {
    assert_eq!(
        included(&fixture(
            "const TEXT: &str = concat!(include_str!(\"guidance/w.md\"), \"x\");",
            1,
            ""
        ))
        .unwrap_err(),
        "included data has no named owning item"
    );
    assert_eq!(
        included(&fixture(
            BINDING,
            1,
            "fn path_reader() {\n    let path = \"guidance/w.md\";\n}\n"
        ))
        .unwrap_err(),
        "included data has another literal reader"
    );
    let mut sources = fixture(BINDING, 1, "");
    sources.texts.insert(
        "crates/ordinary/src/reader.rs".into(),
        "const GUIDE: &str = include_str!(\"../../rsid/src/session/guidance/w.md\");\n".into(),
    );
    assert_eq!(
        included(&sources).unwrap_err(),
        "included data has a reader outside traced library modules"
    );
}

#[test]
fn include_item_observer_bound_is_sixty_unique_identities() {
    assert_eq!(included(&fixture(BINDING, 59, "")).unwrap().len(), 60);
    assert_eq!(
        included(&fixture(BINDING, 60, "")).unwrap_err(),
        "included data exceeds the 60-test bound"
    );
}

#[test]
fn include_item_external_caller_bound_is_twelve_and_does_not_recurse_outside() {
    for count in [12, 13] {
        let extra = (0..count)
            .map(|n| format!("fn external_{n}() {{\n    render();\n}}\n"))
            .collect::<String>();
        // The fixture's existing central wrapper is an external caller too.
        let sources = fixture(BINDING, 1, &extra);
        assert_eq!(
            included(&sources).unwrap_err(),
            "included data exceeds the 12-external-caller bound"
        );
    }
    let extra = (0..11)
        .map(|n| format!("fn external_{n}() {{\n    render();\n}}\n"))
        .collect::<String>();
    assert_eq!(
        included(&fixture(BINDING, 1, &extra)).unwrap(),
        ["central_observer", "render_0"]
    );
}

#[test]
fn include_item_requires_ownership_for_every_changed_data_path() {
    let sources = fixture(BINDING, 1, "");
    let index = ItemReferences::new(&sources).unwrap();
    assert_eq!(
        readers::include_item_tests(
            &sources,
            &index,
            &BTreeSet::from([DATA.into(), "crates/rsid/src/session/missing.json".into()])
        )
        .unwrap_err(),
        "changed data has no complete include-item ownership proof"
    );
    assert_eq!(
        readers::include_item_tests(
            &sources,
            &index,
            &BTreeSet::from(["README.md".into(), "crates/rsid/src/session.rs".into()])
        )
        .unwrap(),
        Vec::new()
    );
}

#[test]
fn include_item_tests_can_own_the_include_directly() {
    let mut sources = fixture("", 0, "");
    sources.texts.insert(
        "crates/rsid/src/session/preamble.rs".into(),
        gate(
            "direct_include",
            "let text = include_str!(\"guidance/w.md\");",
        ),
    );
    assert_eq!(included(&sources).unwrap(), ["direct_include"]);
}

#[test]
fn quoted_includes_do_not_supply_missing_data_ownership() {
    let binding = "const TEXT: &str = r#\"include_str!(\"guidance/w.md\")\"#;";
    assert_eq!(
        included(&fixture(binding, 1, "")).unwrap_err(),
        "changed data has no complete include-item ownership proof"
    );
}
