use super::{
    git::Hunk,
    identity::ItemReferences,
    readers, rpc,
    source::SourceTree,
    trace::{Bounds, Stop},
};
use std::collections::BTreeSet;

fn gate(name: &str, body: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-03\"))]\n#[test]\nfn {name}() {{\n    {body}\n}}\n"
    )
}
fn tree(items: &[(&str, &str)]) -> SourceTree {
    SourceTree::new(
        items
            .iter()
            .map(|(p, t)| ((*p).into(), (*t).into()))
            .collect(),
        "linux",
    )
}
fn dispatcher(count: usize) -> String {
    format!(
        "mod tests;\nimpl RpcServer {{\n    fn dispatch(&self) {{\n        let result = match request.method.as_str() {{\n{}            _ => unknown(),\n        }};\n    }}\n}}\n",
        (0..count)
            .map(|n| format!("            \"Verb{n}\" => self.handle_{n}(request).await,\n"))
            .collect::<String>()
    )
}
fn fixture(text: &str, extra: &[(&str, &str)]) -> SourceTree {
    let mut tests = (0..70)
        .map(|n| gate(&format!("unrelated_{n}"), "dispatch();"))
        .collect::<String>();
    tests.push_str("const RPC_SOURCE: &str = include_str!(\"../rpc.rs\");\nconst EXAMPLE: &str = r#\"include_str!(\"../rpc.rs\")\"#;\nfn dispatch_methods() {\n    let source = RPC_SOURCE;\n}\n");
    for (n, b) in [
        ("dispatch_table_complete", "dispatch_methods();"),
        ("rpc_catalog", "dispatch_methods();"),
        ("export_works", "handle_export();"),
        ("export_refused", "let method = \"ExportThing\";"),
    ] {
        tests.push_str(&gate(n, b));
    }
    let mut files = vec![
        ("crates/rsid/src/lib.rs", "pub mod rpc;\n"),
        (rpc::RPC, text),
        ("crates/rsid/src/rpc/tests.rs", tests.as_str()),
    ];
    files.extend_from_slice(extra);
    tree(&files)
}
fn addition(before: &str, insert: &str) -> (String, Vec<Hunk>) {
    let line = before
        .lines()
        .position(|r| r.trim().starts_with("_ =>"))
        .unwrap()
        + 1;
    (
        before.replace("            _ =>", &format!("{insert}            _ =>")),
        vec![Hunk {
            old_first: line - 1,
            old_count: 0,
            new_first: line,
            new_count: insert.lines().count(),
        }],
    )
}
const ARM: &str = "            \"ExportThing\" => {\n                self.handle_export(request)\n                    .await\n            }\n";

#[test]
fn multiline_rpc_additions_name_runtime_and_table_readers_without_dispatcher_fanout() {
    for count in [5, 70] {
        let before = dispatcher(count);
        let (after, hunks) = addition(&before, ARM);
        let sources = fixture(&after, &[]);
        let mut index = ItemReferences::new(&sources).unwrap();
        let trace = rpc::changed_dispatch_trace(
            &sources,
            &fixture(&before, &[]),
            &mut index,
            rpc::RPC,
            &hunks,
            Bounds::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(trace.stop, None);
        assert_eq!(
            trace
                .tests
                .unwrap()
                .into_iter()
                .map(|o| o.name)
                .collect::<Vec<_>>(),
            [
                "dispatch_table_complete",
                "export_refused",
                "export_works",
                "rpc_catalog"
            ]
        );
        for (observer, distance) in trace.direct {
            if observer.name.starts_with("export_") {
                assert_eq!(distance, 1);
            }
        }
    }
}

#[test]
fn rpc_removals_and_replacements_union_both_pinned_method_sets() {
    let before = dispatcher(2).replace(
        "\"Verb0\" => self.handle_0",
        "\"OldVerb\" => self.handle_old",
    );
    let after = before.replace(
        "\"OldVerb\" => self.handle_old",
        "\"NewVerb\" => self.handle_new",
    );
    let tests = format!(
        "{}{}",
        gate("old_contract", "let method = \"OldVerb\";"),
        gate("new_contract", "handle_new();")
    );
    let extras = [("crates/rsid/src/rpc/contracts.rs", tests.as_str())];
    let sources = fixture(&after, &extras);
    let mut index = ItemReferences::new(&sources).unwrap();
    let trace = rpc::changed_dispatch_trace(
        &sources,
        &fixture(&before, &extras),
        &mut index,
        rpc::RPC,
        &[Hunk {
            old_first: 5,
            old_count: 1,
            new_first: 5,
            new_count: 1,
        }],
        Bounds::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        trace
            .tests
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        [
            "new_contract",
            "old_contract",
            "dispatch_table_complete",
            "rpc_catalog"
        ]
    );
}

#[test]
fn rpc_prelude_unknown_arm_and_executable_registration_edits_require_broad_coverage() {
    let before = dispatcher(2);
    for after in [
        before.replace(
            "        let result =",
            "        shared_gate();\n        let result =",
        ),
        before.replace("_ => unknown()", "_ => new_unknown()"),
        before.replace(
            "self.handle_0(request).await",
            "{ side_effect(); self.handle_0(request).await }",
        ),
    ] {
        let sources = fixture(&after, &[]);
        let mut index = ItemReferences::new(&sources).unwrap();
        let trace = rpc::changed_dispatch_trace(
            &sources,
            &fixture(&before, &[]),
            &mut index,
            rpc::RPC,
            &[Hunk {
                old_first: 4,
                old_count: before.lines().count() - 6,
                new_first: 4,
                new_count: after.lines().count() - 6,
            }],
            Bounds::default(),
        );
        assert_eq!(
            trace.unwrap_err(),
            "RPC edit extends beyond simple method-to-handler registration arms"
        );
    }
}

#[test]
fn literal_alternative_methods_and_no_argument_handlers_are_proven() {
    let before = dispatcher(0);
    let arm = "            \"ExportThing\" | \"ImportThing\" => self.handle_export(),\n";
    let (after, hunks) = addition(&before, arm);
    let sources = fixture(&after, &[]);
    let lines = super::selection::changed_lines(&hunks, false);
    assert_eq!(
        rpc::dispatch_seeds(&after, &lines, &sources).unwrap(),
        BTreeSet::from([
            "ExportThing".into(),
            "ImportThing".into(),
            "handle_export".into()
        ])
    );
}

#[test]
fn qualified_rpc_families_follow_private_exported_and_aliased_request_helpers() {
    let before = dispatcher(70);
    let arm = "            method if sessions::is_operator_method(method) => {\n                self.handle_pending(request).await\n            }\n";
    let (after, hunks) = addition(&before, arm);
    for visibility in ["", "pub(crate) "] {
        let family = format!(
            "pub(super) fn is_operator_method(method: &str) -> bool {{\n    method == \"AnswerPending\"\n}}\n{visibility}fn request() {{\n    let method = \"AnswerPending\";\n}}\n{}",
            gate("pending_answer", "request();")
        );
        let unrelated = format!(
            "fn is_operator_method() {{\n    other_methods();\n}}\nfn request() {{\n    unrelated();\n}}\nfn fixture() {{\n    request();\n}}\n{}",
            (0..70)
                .map(|n| gate(&format!("peer_{n}"), "fixture();"))
                .collect::<String>()
        );
        let loader = format!(
            "use super::sessions::request as session_request;\npub(crate) fn load() {{\n    session_request();\n}}\n{}",
            gate("loaded", "load();")
        );
        let extras = [
            ("crates/rsid/src/rpc/sessions.rs", family.as_str()),
            ("crates/rsid/src/rpc/unrelated.rs", unrelated.as_str()),
            ("crates/rsid/src/rpc/loader.rs", loader.as_str()),
        ];
        let sources = fixture(&after, &extras);
        let mut index = ItemReferences::new(&sources).unwrap();
        let trace = rpc::changed_dispatch_trace(
            &sources,
            &fixture(&before, &extras),
            &mut index,
            rpc::RPC,
            &hunks,
            Bounds::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(trace.stop, None, "{visibility}");
        let expected = if visibility.is_empty() {
            vec!["pending_answer", "dispatch_table_complete", "rpc_catalog"]
        } else {
            vec![
                "loaded",
                "pending_answer",
                "dispatch_table_complete",
                "rpc_catalog",
            ]
        };
        assert_eq!(
            trace
                .tests
                .unwrap()
                .into_iter()
                .map(|o| o.name)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn missing_ambiguous_and_empty_rpc_family_predicates_refuse() {
    let before = dispatcher(0);
    let (after, hunks) = addition(
        &before,
        "            method if sessions::is_operator_method(method) => self.handle_pending(request),\n",
    );
    for family in [
        "",
        "fn is_operator_method() {}\n",
        "fn is_operator_method() {\n    \"Pending\"\n}\nfn is_operator_method() {\n    \"Other\"\n}\n",
    ] {
        let sources = fixture(&after, &[("crates/rsid/src/rpc/sessions.rs", family)]);
        assert!(
            rpc::dispatch_seeds(
                &after,
                &super::selection::changed_lines(&hunks, false),
                &sources
            )
            .is_err()
        );
    }
}

#[test]
fn dispatch_function_and_test_bounds_preserve_direct_priorities_and_none() {
    let text = format!(
        "fn helper() {{\n    handle_export();\n}}\nfn wrapper() {{\n    helper();\n}}\n{}{}",
        gate("direct", "handle_export();"),
        gate("far", "wrapper();")
    );
    let sources = tree(&[("crates/rsid/src/rpc/tests.rs", &text)]);
    let mut index = ItemReferences::new(&sources).unwrap();
    let seeds = BTreeSet::from(["handle_export".into()]);
    let trace = rpc::dispatch_observers(
        &mut index,
        &seeds,
        &BTreeSet::new(),
        Bounds {
            functions: 1,
            ..Bounds::default()
        },
    );
    assert_eq!(trace.stop, Some(Stop::FunctionBound));
    assert_eq!(trace.tests, None);
    assert_eq!(
        trace
            .direct
            .keys()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>(),
        ["direct"]
    );
    let trace = rpc::dispatch_observers(
        &mut index,
        &seeds,
        &BTreeSet::new(),
        Bounds {
            tests: 1,
            ..Bounds::default()
        },
    );
    assert_eq!(trace.stop, Some(Stop::TestBound));
    assert_eq!(trace.tests, None);
    assert_eq!(trace.direct.values().copied().collect::<Vec<_>>(), [1, 3]);
}

#[test]
fn unknown_exported_registration_helper_identity_keeps_runtime_coverage_broad() {
    let text = format!(
        "pub fn request() {{\n    handle_export();\n}}\n{}",
        gate("direct", "handle_export();")
    );
    let sources = tree(&[
        ("crates/rsid/src/rpc/sessions.rs", &text),
        (
            "crates/rsid/src/rpc/peer.rs",
            "pub fn request() {\n    unrelated();\n}\n",
        ),
        (
            "crates/rsid/src/rpc/unknown.rs",
            "fn caller() {\n    crate::missing::request();\n}\n",
        ),
    ]);
    let mut index = ItemReferences::new(&sources).unwrap();
    let trace = rpc::dispatch_observers(
        &mut index,
        &BTreeSet::from(["handle_export".into()]),
        &BTreeSet::new(),
        Bounds::default(),
    );
    assert_eq!(trace.stop, Some(Stop::UnresolvedIdentity));
    assert_eq!(trace.tests, None);
    assert_eq!(trace.direct.len(), 1);
}

#[test]
fn receiver_methods_do_not_alias_exported_free_registration_helpers() {
    let text = format!(
        "pub fn request() {{\n    handle_export();\n}}\n{}",
        gate("direct", "handle_export();")
    );
    let sources = tree(&[
        ("crates/rsid/src/rpc/sessions.rs", &text),
        (
            "crates/rsid/src/rpc/peer.rs",
            "pub fn request() {\n    unrelated();\n}\n",
        ),
        (
            "crates/rsid/src/rpc/unknown.rs",
            "fn caller() {\n    receiver.request();\n}\n",
        ),
    ]);
    let mut index = ItemReferences::new(&sources).unwrap();
    let trace = rpc::dispatch_observers(
        &mut index,
        &BTreeSet::from(["handle_export".into()]),
        &BTreeSet::new(),
        Bounds::default(),
    );
    assert_eq!(trace.stop, None);
    assert_eq!(
        trace
            .tests
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        ["direct"]
    );
}

#[test]
fn non_dispatcher_rpc_edits_delegate_to_the_generic_seed_proof() {
    let before = format!("{}\nfn ordinary() {{\n    old();\n}}\n", dispatcher(0));
    let after = before.replace("    old();", "    new();");
    let sources = fixture(&after, &[]);
    let mut index = ItemReferences::new(&sources).unwrap();
    let line = after.lines().position(|r| r.trim() == "new();").unwrap() + 1;
    assert!(
        rpc::changed_dispatch_trace(
            &sources,
            &fixture(&before, &[]),
            &mut index,
            rpc::RPC,
            &[Hunk {
                old_first: line,
                old_count: 1,
                new_first: line,
                new_count: 1
            }],
            Bounds::default()
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn lexical_includes_exclude_comments_raw_examples_and_byte_fixture_strings() {
    let text = "// include_str!(\"wrong\")\nconst EXAMPLE: &str = r#\"include_str!(\"wrong\")\"#;\nconst BYTES: &[u8] = b\"include_bytes!(\\\"wrong\\\")\";\nconst TEXT: &str = include_str!(\n    \"../rpc.rs\"\n);\n";
    let includes = readers::code_includes(text).unwrap();
    assert_eq!(
        includes
            .iter()
            .map(|i| i.target.as_str())
            .collect::<Vec<_>>(),
        ["../rpc.rs"]
    );
    assert_eq!(
        &text[includes[0].offset..includes[0].offset + 8],
        "include_"
    );
    assert!(readers::code_includes("const BROKEN: &str = r#\"unclosed").is_err());
}

#[test]
fn named_source_constants_follow_long_helpers_and_unique_cross_module_callers() {
    let source = format!(
        "const SOURCE: &str = include_str!(\"../rpc.rs\");\nfn scan() {{\n    let text = SOURCE;\n{}\n}}\n",
        "    inspect();\n".repeat(200)
    );
    let observer = format!(
        "fn wrapper() {{\n    scan();\n}}\n{}",
        gate("source_observer", "wrapper();")
    );
    let sources = tree(&[
        ("crates/rsid/src/rpc/reader.rs", &source),
        ("crates/rsid/src/rpc/contracts.rs", &observer),
    ]);
    let index = ItemReferences::new(&sources).unwrap();
    assert_eq!(
        readers::source_scanner_tests(&sources, &index, &BTreeSet::from([rpc::RPC.into()]))
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        ["source_observer"]
    );
}

#[test]
fn function_local_and_unnamed_source_includes_keep_their_observers() {
    for (binding, expected) in [
        (
            "fn scan() {\n    include_bytes!(\"../rpc.rs\");\n}\n",
            vec!["direct"],
        ),
        (
            "source_macro!(include_str!(\"../rpc.rs\"));\n",
            vec!["direct", "other"],
        ),
    ] {
        let text = format!(
            "{binding}{}{}",
            gate("direct", "scan();"),
            gate("other", "unrelated();")
        );
        let sources = tree(&[("crates/rsid/src/rpc/reader.rs", &text)]);
        let index = ItemReferences::new(&sources).unwrap();
        assert_eq!(
            readers::source_scanner_tests(&sources, &index, &BTreeSet::from([rpc::RPC.into()]))
                .unwrap()
                .into_iter()
                .map(|o| o.name)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn source_read_helpers_with_shared_names_stay_in_their_module_subtree() {
    let reader = format!(
        "fn scan() {{\n    include_str!(\"../rpc.rs\");\n}}\n{}",
        gate("source_observer", "scan();")
    );
    let peer = format!(
        "fn scan() {{\n    unrelated();\n}}\n{}",
        gate("peer_observer", "scan();")
    );
    let sources = tree(&[
        ("crates/rsid/src/rpc/reader.rs", &reader),
        ("crates/rsid/src/rpc/peer.rs", &peer),
    ]);
    let index = ItemReferences::new(&sources).unwrap();
    assert_eq!(
        readers::source_scanner_tests(&sources, &index, &BTreeSet::from([rpc::RPC.into()]))
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        ["source_observer"]
    );
}

#[test]
fn rpc_headers_quoted_in_fixtures_never_create_a_dispatcher_match() {
    let text = format!(
        "const EXAMPLE: &str = r#\"match request.method.as_str() {{\n}}\"#;\n{}",
        dispatcher(0)
    );
    let sources = fixture(&text, &[]);
    assert_eq!(
        rpc::dispatch_seeds(&text, &BTreeSet::new(), &sources).unwrap(),
        BTreeSet::new()
    );
}
