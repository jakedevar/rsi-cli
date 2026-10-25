//! API-level intents of the legacy suite: gates, lexemes, runnable-test proofs,
//! per-host cfg evaluation, mount closures and the rewind fast path.
use super::derive_tests::gate;
use super::source::{
    self, SourceTree, code_shards, holds_runnable_test, mod_declarations, module_map, mount_closure,
};
use std::collections::{BTreeMap, BTreeSet};

fn names(text: &str, host: &str) -> Vec<String> {
    source::rust_functions(text, host)
        .into_iter()
        .map(|f| f.name)
        .collect()
}

#[test]
fn only_cfg_gates_in_code_count() {
    let fixture = "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-01\"))]\n#[test]\nfn t() {\n    let script = \"#[cfg(any(feature = \\\"test-shard-store-04\\\"))]\";\n    let raw = r#\"feature = \"test-shard-session-05\"\"#;\n    let ch = '\"'; let lt: &'a str = \"x\";\n}\n// feature = \"test-shard-store-01\"\n/* /* nested */ feature = \"test-shard-other-05\" */\n";
    assert_eq!(
        code_shards(fixture).unwrap(),
        BTreeSet::from(["other-01".to_owned()])
    );
}

#[test]
fn helper_gate_listing_several_shards_all_count() {
    let text = "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\", feature = \"test-shard-store-02\"))]\nfn h() {}\n";
    assert_eq!(
        code_shards(text).unwrap(),
        BTreeSet::from(["store-01".to_owned(), "store-02".to_owned()])
    );
}

#[test]
fn a_comment_ending_in_a_quote_does_not_open_a_string() {
    let text = "// echoes it as a \"user\"\nlet a = \"user\".to_string();\nlet b = \"x\";\n";
    let found: Vec<_> = source::rust_lexemes(text)
        .unwrap()
        .iter()
        .map(|l| (l.kind, &text[l.start..l.end]))
        .collect();
    assert_eq!(
        found,
        [
            (source::LexemeKind::Comment, "// echoes it as a \"user\""),
            (source::LexemeKind::Literal, "\"user\""),
            (source::LexemeKind::Literal, "\"x\"")
        ]
    );
}

#[test]
fn unrecognized_test_attribute_beside_an_ignored_test_keeps_the_filter() {
    let ignored = "#[test]\n#[ignore = \"slow\"]\nfn slow() {}\n";
    for other in [
        "#[cfg_attr(test, test)]\nfn live() {}\n",
        "#[rstest]\nfn live() {}\n",
        "#[test_case(1)]\nfn live(n: u8) {}\n",
        "#[proptest]\nfn live() {}\n",
        "#[some::path::test]\nfn live() {}\n",
        "proptest! { fn live() {} }\n",
    ] {
        assert!(
            holds_runnable_test(&format!("{ignored}{other}"), "linux").unwrap(),
            "{other}"
        );
        assert!(holds_runnable_test(other, "linux").unwrap(), "{other}");
    }
}

#[test]
fn only_proven_empty_sources_drop_the_filter() {
    for text in [
        "#[test]\n#[ignore]\nfn a() {}\n",
        "#[tokio::test]\n#[ignore = \"x\"]\nasync fn a() {}\n",
        "/// a `#[rstest]` and `#[cfg_attr(test, test)]` in prose\nfn a() {}\n",
        "const S: &str = \"#[test] #[rstest]\";\n",
        "#[cfg_attr(test, derive(Debug))]\nstruct S;\n#[cfg(test)]\nmod t {}\n",
    ] {
        assert!(!holds_runnable_test(text, "linux").unwrap(), "{text}");
    }
    assert!(
        holds_runnable_test(
            "#[test]\n#[ignore]\nfn a() {}\n#[test]\nfn b() {}\n",
            "linux"
        )
        .unwrap()
    );
}

const INLINE: &str = "#[test]\nfn everywhere() {}\n\n#[cfg(target_os = \"macos\")]\nmod mac_only {\n    use super::*;\n\n    #[test]\n    fn mac_nested() {}\n\n    mod deeper {\n        #[test]\n        fn mac_deeper() {}\n    }\n}\n\n#[cfg(all(test, target_os = \"windows\"))]\nmod win_only {\n    #[test]\n    fn win_nested() {}\n}\n\n#[cfg(not(target_os = \"windows\"))]\n#[allow(dead_code)]\nmod not_windows {\n    #[test]\n    fn not_win_nested() {}\n}\n\n#[cfg(feature = \"x\")]\nmod unknown_feature {\n    #[test]\n    fn feature_gated() {}\n}\n\n#[cfg(any(target_os = \"macos\", target_os = \"windows\"))]\nmod any_os {\n    #[test]\n    fn any_nested() {}\n}\n\n#[cfg(target_os = \"macos\")]\nmod never_closed {\n    #[test]\n    fn unclosed() {}\n#[test]\nfn after_modules() {}\n";

#[test]
fn a_linux_host_keeps_only_the_tests_it_compiles() {
    let text = "#[cfg(target_os = \"macos\")]\n#[test]\nfn mac_only() {}\n#[cfg(not(target_os = \"linux\"))]\n#[test]\nfn not_linux() {}\n#[cfg(target_os = \"linux\")]\n#[test]\nfn linux_only() {}\n#[cfg(all(target_os = \"macos\", unix))]\n#[test]\nfn compound() {}\n#[test]\nfn everywhere() {}\n";
    assert_eq!(names(text, "linux"), ["linux_only", "everywhere"]);
    assert_eq!(
        names(text, "macos"),
        ["mac_only", "not_linux", "compound", "everywhere"]
    );
}

#[test]
fn an_inline_module_gate_leaves_out_its_tests_on_each_host() {
    // `never_closed` has no closing brace at its indentation: nothing is proven, so its test stays.
    assert_eq!(
        names(INLINE, "linux"),
        [
            "everywhere",
            "not_win_nested",
            "feature_gated",
            "unclosed",
            "after_modules"
        ]
    );
    assert_eq!(
        names(INLINE, "macos"),
        [
            "everywhere",
            "mac_nested",
            "mac_deeper",
            "not_win_nested",
            "feature_gated",
            "any_nested",
            "unclosed",
            "after_modules"
        ]
    );
    assert_eq!(
        names(INLINE, "windows"),
        [
            "everywhere",
            "win_nested",
            "feature_gated",
            "any_nested",
            "unclosed",
            "after_modules"
        ]
    );
}

#[test]
fn a_files_own_inner_gate_leaves_out_all_its_tests() {
    let text = "// header\n\n#![allow(dead_code)]\n#![cfg(target_os = \"macos\")]\n#[test]\nfn mac_file() {}\n";
    assert!(names(text, "linux").is_empty());
    assert_eq!(names(text, "macos"), ["mac_file"]);
    let late = "#[test]\nfn first() {}\n#![cfg(target_os = \"macos\")]\n";
    assert_eq!(names(late, "linux"), ["first"]);
}

const EXTERNAL_ROOT: &str = "mod live;\n#[cfg(target_os = \"macos\")]\nmod mac_dir;\n#[cfg(target_os = \"windows\")]\n#[path = \"win_impl.rs\"]\nmod win;\n#[cfg(target_os = \"macos\")] mod same_line;\n#[cfg(target_os = \"macos\")]\n#[path = \"shared.rs\"]\nmod mac_view;\nmod shared;\n#[cfg(target_os = \"windows\")]\nmod win_wrap {\n    mod inner;\n}\n#[cfg(feature = \"x\")]\nmod unknown;\n";

fn gated_off(host: &str) -> Vec<String> {
    let mut files = BTreeMap::from([("crates/x/src/lib.rs".to_owned(), EXTERNAL_ROOT.to_owned())]);
    for rel in [
        "live",
        "mac_dir",
        "mac_dir/child",
        "win_impl",
        "same_line",
        "shared",
        "win_wrap/inner",
        "unknown",
    ] {
        files.insert(
            format!("crates/x/src/{rel}.rs"),
            "#[test]\nfn t() {}\n".into(),
        );
    }
    files.insert(
        "crates/x/src/mac_dir.rs".into(),
        "mod child;\n#[test]\nfn t() {}\n".into(),
    );
    module_map(&files, host)
        .gated_off
        .iter()
        .map(|p| p.trim_start_matches("crates/x/src/").to_owned())
        .collect()
}

#[test]
fn an_external_module_declaration_gate_leaves_out_the_file_and_its_children() {
    assert_eq!(
        gated_off("linux"),
        [
            "mac_dir.rs",
            "mac_dir/child.rs",
            "same_line.rs",
            "win_impl.rs",
            "win_wrap/inner.rs"
        ]
    );
    assert_eq!(gated_off("macos"), ["win_impl.rs", "win_wrap/inner.rs"]);
    assert_eq!(
        gated_off("windows"),
        ["mac_dir.rs", "mac_dir/child.rs", "same_line.rs"]
    );
}

#[test]
fn a_file_also_mounted_without_a_gate_stays_in() {
    for host in ["linux", "macos", "windows"] {
        assert!(!gated_off(host).contains(&"shared.rs".to_owned()), "{host}");
    }
}

#[test]
fn a_marked_source_yields_no_functions_and_no_runnable_test() {
    let text = "#[test]\nfn t() {}\n";
    let marked = format!("{text}{}", source::HOST_GATED_OFF);
    assert!(names(&marked, "linux").is_empty());
    assert!(holds_runnable_test(text, "linux").unwrap());
    assert!(!holds_runnable_test(&marked, "linux").unwrap());
    assert!(marked.starts_with(text));
}

#[test]
fn a_source_whose_only_tests_are_gated_off_holds_no_runnable_test() {
    let mac_test = "#[cfg(target_os = \"macos\")]\n#[test]\nfn a() {}\n";
    let mac_mod =
        "#[cfg(all(test, target_os = \"macos\"))]\nmod t {\n    #[test]\n    fn a() {}\n}\n";
    let live = "#[test]\nfn b() {}\n";
    let unknown = "#[cfg(feature = \"x\")]\n#[test]\nfn c() {}\n";
    let ignored = "#[test]\n#[ignore]\nfn d() {}\n";
    let linux = [
        (mac_test.to_owned(), false),
        (mac_mod.to_owned(), false),
        (format!("{mac_test}{live}"), true),
        (format!("{mac_mod}{live}"), true),
        (format!("{mac_test}{unknown}"), true),
        (format!("{mac_test}{ignored}"), false),
    ];
    for (text, expected) in linux {
        assert_eq!(
            holds_runnable_test(&text, "linux").unwrap(),
            expected,
            "linux {text}"
        );
    }
    for (text, expected) in [
        (mac_test.to_owned(), true),
        (mac_mod.to_owned(), true),
        (format!("{mac_test}{ignored}"), true),
    ] {
        assert_eq!(
            holds_runnable_test(&text, "macos").unwrap(),
            expected,
            "macos {text}"
        );
    }
}

#[test]
fn the_shared_gate_grammar_answers_as_check_rsid_test_shards_does() {
    let rows = [
        "#[cfg(target_os = \"macos\")]",
        "#[cfg(target_os = \"linux\")]",
        "#[cfg(target_os = \"windows\")]",
        "#[cfg(not(target_os = \"macos\"))]",
        "#[cfg(not(target_os = \"linux\"))]",
        "#[cfg(all(test, target_os = \"macos\"))]",
        "#[cfg(all(test, not(target_os = \"linux\")))]",
        "#[cfg(feature = \"x\")]",
        "#[cfg(target_os = \"macos\"]",
        "#[cfg(test)]",
        "#[test]",
    ];
    for host in ["linux", "macos", "windows"] {
        let platform = crate::rsid_test_shards::Host {
            os: host.into(),
            family: Some(if host == "windows" { "windows" } else { "unix" }.into()),
        };
        for row in rows {
            assert_eq!(
                source::other_os_gate(row, host),
                crate::rsid_test_shards::cfg::host_excluded(row, &platform),
                "{host} {row}"
            );
        }
    }
}

#[test]
fn rewind_only_change_matches_full_matching() {
    let helper = "fn rewind_post_v121_tail_to() {\n    step_a();\n    step_b();\n}\n";
    let other = "fn unrelated() {\n    step_a();\n}\n";
    let constant = |n: u8| format!("const REWIND_TEARDOWN_COVERED_THROUGH: i32 = {n};\n");
    let cases = [
        (
            format!("{other}{helper}"),
            format!("{other}{}", helper.replace("step_b", "step_c")),
            true,
        ),
        (
            format!("{other}{helper}"),
            format!("{}{helper}", other.replace("step_a", "step_z")),
            false,
        ),
        (
            format!("{other}{helper}{}", constant(1)),
            format!("{other}{helper}{}", constant(2)),
            true,
        ),
        (
            format!("{other}{helper}"),
            format!("{other}{helper}"),
            false,
        ),
        (
            format!("{other}{helper}"),
            format!("{helper}{other}"),
            false,
        ),
        (
            format!("{other}{other}{helper}"),
            format!("{other}{helper}"),
            false,
        ),
        (
            format!("{other}{helper}"),
            format!("{other}{helper}{other}"),
            false,
        ),
        (
            format!("{other}{helper}"),
            format!(
                "fn extra() {{}}\n{other}{}",
                helper.replace("step_b", "step_c")
            ),
            false,
        ),
    ];
    for (before, after, expected) in cases {
        assert_eq!(
            super::migration::rewind_only_change(&before, &after),
            expected,
            "{before:?} -> {after:?}"
        );
    }
}

#[test]
fn mod_declarations_is_stable_across_calls() {
    let text =
        "#[path = \"x.rs\"]\nmod a;\nmod b {\n    mod c;\n}\ninclude!(\"y.rs\");\nlet s = 1;\n";
    let found: Vec<_> = mod_declarations(text, "linux")
        .into_iter()
        .map(|d| (d.name, d.path, d.inline, d.off))
        .collect();
    assert_eq!(
        found,
        [
            (Some("a".to_owned()), Some("x.rs".to_owned()), vec![], false),
            (Some("c".to_owned()), None, vec!["b".to_owned()], false),
            (None, Some("y.rs".to_owned()), vec![], false),
        ]
    );
    assert_eq!(mod_declarations(text, "linux").len(), 3);
}

fn closure(declaration: &str, extra: &[&str]) -> BTreeMap<String, String> {
    let src = "crates/x/src/";
    let mut files = BTreeMap::from([
        (format!("{src}lib.rs"), "pub mod store;\n".to_owned()),
        (
            format!("{src}store/mod.rs"),
            "pub mod pending;\n".to_owned(),
        ),
        (
            format!("{src}store/pending.rs"),
            "#[path = \"remote_answers.rs\"]\npub mod remote_answers;\n".to_owned(),
        ),
        (
            format!("{src}store/remote_answers.rs"),
            declaration.to_owned(),
        ),
    ]);
    for name in extra {
        files.insert(format!("{src}{name}"), "// file\n".into());
    }
    mount_closure(&files, &format!("{src}lib.rs"), "linux")
        .modules
        .into_iter()
        .map(|(k, v)| (k.trim_start_matches(src).to_owned(), v))
        .collect()
}

#[test]
fn plain_mod_tests_in_a_path_mounted_file_resolves_beside_it() {
    let mapped = closure(
        "#[cfg(test)]\nmod tests;\n",
        &["store/tests.rs", "store/remote_answers/tests.rs"],
    );
    assert_eq!(
        mapped["store/tests.rs"],
        "store::pending::remote_answers::tests"
    );
    assert!(!mapped.contains_key("store/remote_answers/tests.rs"));
}

#[test]
fn an_explicit_path_test_mount_resolves_to_the_module_directory_file() {
    let mapped = closure(
        "#[cfg(test)]\n#[path = \"remote_answers/tests.rs\"]\nmod tests;\n",
        &["store/tests.rs", "store/remote_answers/tests.rs"],
    );
    assert_eq!(
        mapped["store/remote_answers/tests.rs"],
        "store::pending::remote_answers::tests"
    );
    assert!(!mapped.contains_key("store/tests.rs"));
}

#[test]
fn an_ordinary_mounted_file_keeps_its_stem_directory() {
    let src = "crates/x/src/";
    let files = BTreeMap::from([
        (format!("{src}lib.rs"), "pub mod ordinary;\n".to_owned()),
        (format!("{src}ordinary.rs"), "mod tests;\n".to_owned()),
        (format!("{src}ordinary/tests.rs"), "// t\n".to_owned()),
        (format!("{src}tests.rs"), "// not this one\n".to_owned()),
    ]);
    let mapped = mount_closure(&files, &format!("{src}lib.rs"), "linux").modules;
    assert_eq!(
        mapped
            .get(&format!("{src}ordinary/tests.rs"))
            .map(String::as_str),
        Some("ordinary::tests")
    );
    assert!(!mapped.contains_key(&format!("{src}tests.rs")));
}

#[test]
fn source_tree_marks_gated_files_without_moving_lines() {
    let files = BTreeMap::from([
        (
            "crates/x/src/lib.rs".to_owned(),
            "#[cfg(target_os = \"windows\")]\nmod w;\n".to_owned(),
        ),
        ("crates/x/src/w.rs".to_owned(), gate("store-01", "w")),
    ]);
    let tree = SourceTree::new(files, "linux");
    let text = &tree.texts["crates/x/src/w.rs"];
    assert!(text.starts_with(&gate("store-01", "w")));
    assert!(text.ends_with(source::HOST_GATED_OFF));
}

#[test]
fn target_inventories_are_built_once_per_tree_not_once_per_changed_path() {
    use std::sync::atomic::Ordering::Relaxed;
    let (modules, targets) = (20, 8);
    let mut files = BTreeMap::new();
    let lib: String = (0..modules).map(|i| format!("pub mod m{i};\n")).collect();
    files.insert("crates/x/src/lib.rs".to_owned(), lib);
    for i in 0..modules {
        files.insert(format!("crates/x/src/m{i}.rs"), "// module\n".to_owned());
    }
    for k in 0..targets {
        files.insert(
            format!("crates/x/tests/t{k}.rs"),
            "mod helpers;\n#[test]\nfn runs() {}\n".to_owned(),
        );
    }
    files.insert(
        "crates/x/tests/helpers/mod.rs".to_owned(),
        "// shared\n".to_owned(),
    );
    let tree = SourceTree::new(files, "linux");
    for i in 0..modules {
        let owners = tree.target_owners(&format!("crates/x/src/m{i}.rs"));
        assert!(owners.is_empty(), "{owners:?}");
        let owners = tree.target_owners("crates/x/tests/helpers/mod.rs");
        assert_eq!(owners.len(), targets, "{owners:?}");
    }
    for target in tree.targets() {
        assert!(tree.target_declares_tests(&target).unwrap());
        assert!(tree.target_declares_tests(&target).unwrap());
    }
    // 20 library paths and 20 helper queries over 8 targets: one inventory scan,
    // one module closure per target, however many paths ask.
    assert!(
        tree.caches.mount_builds.load(Relaxed) <= targets,
        "{} module closures",
        tree.caches.mount_builds.load(Relaxed)
    );
    assert!(
        tree.caches.target_scans.load(Relaxed) <= 2,
        "{} target scans (one for the queries, one for this loop)",
        tree.caches.target_scans.load(Relaxed)
    );
}
