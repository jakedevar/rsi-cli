use super::{
    git::Hunk,
    identity::ItemReferences,
    leaf::{self, LeafObservers},
    source::SourceTree,
    trace::{Bounds, Stop},
};
use std::collections::BTreeMap;

const PATH: &str = "crates/rsid-remote-read/src/projection.rs";
const PROJECTION: &str = "pub fn project_pending() -> u32 {\n    1\n}\n";
fn gate(shard: &str, name: &str, body: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{\n    {body}\n}}\n"
    )
}
fn fixture(path: &str, text: &str, facade: &str, extra: &str) -> SourceTree {
    let owner = format!(
        "{}pub fn spawn_info_read() -> u32 {{\n    project_pending()\n}}\n",
        gate("store-04", "owner_reads_projection", "spawn_info_read();")
    );
    let query = format!(
        "{}pub fn session_probe() -> u32 {{\n    project_pending()\n}}\n",
        gate(
            "session-01",
            "session_probe_sees_projection",
            "session_probe();"
        )
    );
    let package = path.split('/').nth(1).unwrap();
    SourceTree::new(
        BTreeMap::from([
            (
                format!("crates/{package}/src/lib.rs"),
                "mod projection;\npub use projection::project_pending;\n".into(),
            ),
            (path.into(), text.into()),
            (
                "crates/rsid/src/lib.rs".into(),
                "pub mod session;\npub mod remote_read;\npub mod topology;\nmod other;\n".into(),
            ),
            (
                format!("crates/rsid/src/{facade}/mod.rs"),
                "mod owner;\n".into(),
            ),
            (format!("crates/rsid/src/{facade}/owner.rs"), owner),
            ("crates/rsid/src/session/queries.rs".into(), query),
            (
                "crates/rsid/src/other.rs".into(),
                format!(
                    "{}{extra}",
                    gate("other-01", "unrelated_daemon_test", "unrelated();")
                ),
            ),
        ]),
        "linux",
    )
}
fn change(before: &str, after: &str) -> Vec<Hunk> {
    let old = before.lines().position(|r| r.trim() == "1").unwrap() + 1;
    let new = after.lines().position(|r| r.trim() == "2").unwrap() + 1;
    vec![Hunk {
        old_first: old,
        old_count: 1,
        new_first: new,
        new_count: 1,
    }]
}
fn selection(path: &str, before: &str, after: &str, facade: &str, extra: &str) -> LeafObservers {
    let sources = fixture(path, after, facade, extra);
    let old = fixture(path, before, facade, extra);
    let mut index = ItemReferences::new(&sources).unwrap();
    leaf::daemon_observers(
        &sources,
        &old,
        &mut index,
        path,
        &change(before, after),
        false,
        Bounds::default(),
    )
    .unwrap()
    .unwrap()
}

#[test]
fn remote_read_leaf_keeps_its_facade_and_exact_daemon_observers() {
    let note = selection(
        PATH,
        PROJECTION,
        &PROJECTION.replace("    1", "    2"),
        "remote_read",
        "",
    );
    assert_eq!(note.package, "rsid-remote-read");
    assert_eq!(note.path, PATH);
    assert_eq!(note.unfiltered, false);
    assert_eq!(note.filters.len(), 3);
    assert_eq!(
        note.facades[0].filter,
        "rsid=shard:store-04:test(remote_read)"
    );
    assert_eq!(
        note.trace
            .unwrap()
            .tests
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        ["owner_reads_projection", "session_probe_sees_projection"]
    );
}

#[test]
fn harness_top_level_and_nested_tools_keep_the_retained_harness_facade() {
    for path in [
        "crates/rsid-harness/src/arguments.rs",
        "crates/rsid-harness/src/tools/file.rs",
    ] {
        let note = selection(
            path,
            PROJECTION,
            &PROJECTION.replace("    1", "    2"),
            "session/harness",
            "",
        );
        assert_eq!(note.package, "rsid-harness");
        assert_eq!(note.unfiltered, false);
        assert_eq!(
            note.facades[0].filter,
            "rsid=shard:store-04:test(session::harness)"
        );
        assert_eq!(note.trace.unwrap().tests.unwrap().len(), 2);
    }
}

#[test]
fn private_leaf_helpers_follow_public_wrappers_and_deep_chains() {
    for body in [
        "fn private_projection() -> u32 {\n    1\n}\npub fn project_pending() -> u32 {\n    private_projection()\n}\n",
        "fn private_projection() -> u32 {\n    1\n}\nfn intermediate() -> u32 {\n    private_projection()\n}\npub fn project_pending() -> u32 {\n    intermediate()\n}\n",
    ] {
        let note = selection(
            PATH,
            body,
            &body.replace("    1", "    2"),
            "remote_read",
            "",
        );
        assert_eq!(note.unfiltered, false);
        assert_eq!(note.trace.unwrap().tests.unwrap().len(), 2);
    }
}

#[test]
fn leaf_edits_follow_long_leaf_and_daemon_wrappers_with_the_same_bounds() {
    let before = format!(
        "fn private_projection() -> u32 {{\n    1\n}}\npub fn project_pending() -> u32 {{\n{}    private_projection()\n}}\n",
        "    work();\n".repeat(200)
    );
    let after = before.replace("    1", "    2");
    let mut sources = fixture(PATH, &after, "remote_read", "");
    let old = fixture(PATH, &before, "remote_read", "");
    let text = sources
        .texts
        .get_mut("crates/rsid/src/session/queries.rs")
        .unwrap();
    *text = text.replace(
        "    project_pending()",
        &format!("{}    project_pending()", "    inspect();\n".repeat(200)),
    );
    let mut index = ItemReferences::new(&sources).unwrap();
    let note = leaf::daemon_observers(
        &sources,
        &old,
        &mut index,
        PATH,
        &change(&before, &after),
        false,
        Bounds::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(note.unfiltered, false);
    assert_eq!(note.trace.unwrap().tests.unwrap().len(), 2);
}

#[test]
fn unobserved_private_leaf_helpers_settle_empty_and_keep_facade_coverage() {
    let before = format!("fn dead_helper() -> u32 {{\n    1\n}}\n{PROJECTION}");
    let after = before.replacen("    1", "    2", 1);
    let note = selection(PATH, &before, &after, "remote_read", "");
    assert_eq!(note.unfiltered, false);
    assert_eq!(note.filters, ["rsid=shard:store-04:test(remote_read)"]);
    assert_eq!(note.trace.unwrap().tests, Some(Vec::new()));
}

#[test]
fn ambiguous_exported_leaf_functions_require_complete_unfiltered_selection() {
    let before =
        "fn shared() -> u32 {\n    1\n}\npub fn project_pending() -> u32 {\n    shared()\n}\n";
    let note = selection(
        PATH,
        before,
        &before.replace("    1", "    2"),
        "remote_read",
        "pub fn project_pending() -> u32 {\n    unrelated();\n}\n",
    );
    assert_eq!(note.unfiltered, true);
    assert_eq!(note.filters, Vec::<String>::new());
    assert_eq!(note.facades.len(), 0);
    assert_eq!(note.trace.unwrap().stop, Some(Stop::UnresolvedIdentity));
}

#[test]
fn unproved_leaf_exports_and_module_registrations_derive_no_facade_only_gate() {
    let path = "crates/rsid-remote-read/src/lib.rs";
    for (before, after) in [
        (
            "pub use projection::project_pending;\n",
            "pub use projection::{project_pending, other_pending};\n",
        ),
        ("mod projection;\n", "mod projection;\nmod new_module;\n"),
    ] {
        let sources = fixture(path, after, "remote_read", "");
        let old = fixture(path, before, "remote_read", "");
        let mut index = ItemReferences::new(&sources).unwrap();
        let note = leaf::daemon_observers(
            &sources,
            &old,
            &mut index,
            path,
            &[Hunk {
                old_first: 1,
                old_count: before.lines().count(),
                new_first: 1,
                new_count: after.lines().count(),
            }],
            false,
            Bounds::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(note.unfiltered, true);
        assert_eq!(note.filters, Vec::<String>::new());
    }
}

#[test]
fn leaf_test_only_and_comment_only_edits_do_not_add_daemon_facades() {
    let before = format!("{PROJECTION}#[test]\nfn own_test() {{\n    check();\n}}\n");
    let after = before.replace("    check();", "    check_again();");
    let sources = fixture(PATH, &after, "remote_read", "");
    let old = fixture(PATH, &before, "remote_read", "");
    let mut index = ItemReferences::new(&sources).unwrap();
    let note = leaf::daemon_observers(
        &sources,
        &old,
        &mut index,
        PATH,
        &[Hunk {
            old_first: 6,
            old_count: 1,
            new_first: 6,
            new_count: 1,
        }],
        false,
        Bounds::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(note.filters, Vec::<String>::new());
    assert_eq!(note.unfiltered, false);
    assert!(
        leaf::daemon_observers(
            &sources,
            &old,
            &mut index,
            PATH,
            &[],
            true,
            Bounds::default()
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn planning_leaf_uses_exact_observers_and_keeps_broad_facades_for_full_fallback() {
    let path = "crates/rsi-graph/src/planning/plan.rs";
    let note = selection(
        path,
        PROJECTION,
        &PROJECTION.replace("    1", "    2"),
        "topology",
        "",
    );
    assert_eq!(note.package, "rsi-graph");
    assert_eq!(note.unfiltered, false);
    assert_eq!(note.facades.len(), 0);
    assert_eq!(note.filters.len(), 2);
    let outside = "crates/rsi-graph/src/graph.rs";
    let sources = fixture(outside, PROJECTION, "topology", "");
    let mut index = ItemReferences::new(&sources).unwrap();
    assert!(
        leaf::daemon_observers(
            &sources,
            &sources,
            &mut index,
            outside,
            &[],
            false,
            Bounds::default()
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn leaf_trace_modules_prefix_crates_including_their_roots() {
    let sources = fixture(PATH, PROJECTION, "remote_read", "");
    assert_eq!(
        sources.trace_module(PATH).as_deref(),
        Some("rsid_remote_read::projection")
    );
    assert_eq!(
        sources
            .trace_module("crates/rsid-remote-read/src/lib.rs")
            .as_deref(),
        Some("rsid_remote_read")
    );
    assert_eq!(
        sources
            .trace_module("crates/rsid/src/session/queries.rs")
            .as_deref(),
        Some("session::queries")
    );
}

#[test]
fn facade_substrings_anchor_foreign_tests_and_retain_broad_budget_records() {
    let extra = gate("other-01", "unrelated_remote_read_label", "unrelated();");
    let note = selection(
        PATH,
        PROJECTION,
        &PROJECTION.replace("    1", "    2"),
        "remote_read",
        &extra,
    );
    assert_eq!(
        note.facades[0].filter,
        "rsid=shard:store-04:test(/^remote_read::/)"
    );
    assert_eq!(note.facades[0].tests, 1);
    let after = PROJECTION.replace("    1", "    2");
    let mut sources = fixture(PATH, &after, "remote_read", "");
    sources
        .texts
        .get_mut("crates/rsid/src/remote_read/owner.rs")
        .unwrap()
        .push_str(
            &(0..70)
                .map(|n| gate("store-04", &format!("facade_{n}"), "other();"))
                .collect::<String>(),
        );
    let mut index = ItemReferences::new(&sources).unwrap();
    let note = leaf::daemon_observers(
        &sources,
        &fixture(PATH, PROJECTION, "remote_read", ""),
        &mut index,
        PATH,
        &change(PROJECTION, &after),
        false,
        Bounds::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(note.unfiltered, false);
    assert_eq!(note.facades[0].tests, 71);
    assert_eq!(note.facades[0].broad, true);
}

#[test]
fn bounded_leaf_traces_preserve_priority_tests_but_require_full_fallback() {
    let after = PROJECTION.replace("    1", "    2");
    let sources = fixture(PATH, &after, "remote_read", "");
    let mut index = ItemReferences::new(&sources).unwrap();
    let note = leaf::daemon_observers(
        &sources,
        &fixture(PATH, PROJECTION, "remote_read", ""),
        &mut index,
        PATH,
        &change(PROJECTION, &after),
        false,
        Bounds {
            tests: 1,
            ..Bounds::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(note.unfiltered, true);
    assert_eq!(note.filters, Vec::<String>::new());
    let trace = note.trace.unwrap();
    assert_eq!(trace.tests, None);
    assert_eq!(trace.stop, Some(Stop::TestBound));
    assert_eq!(trace.direct.len(), 2);
}
