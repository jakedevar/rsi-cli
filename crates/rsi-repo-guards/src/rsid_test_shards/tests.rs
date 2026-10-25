use super::*;
use crate::testutil::{repo_root, run, write};
use serde_json::{Value, json};
fn source_test(name: &str, shard: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{}}\n"
    )
}
fn files(rows: &[(&str, String)]) -> Sources {
    rows.iter()
        .map(|(p, s)| (format!("crates/rsid/src/{p}"), s.clone()))
        .collect()
}
fn shards() -> Vec<Shard> {
    vec![
        Shard {
            name: "store-01".into(),
            packages: vec!["rsid".into()],
        },
        Shard {
            name: "store-02".into(),
            packages: vec!["rsid".into()],
        },
    ]
}
fn inventory(f: &Sources) -> Inventory {
    source_inventory(f, &shards(), true, &Host::default()).unwrap()
}
fn host(os: &str, family: &str) -> Host {
    Host {
        os: os.into(),
        family: Some(family.into()),
    }
}
fn cargo(root: &Path, package: &str, names: &[&str]) {
    write(
        root,
        &format!("crates/{package}/Cargo.toml"),
        &format!(
            "[features]\ndefault = []\ntest-shard-mode = []\n{}",
            names
                .iter()
                .map(|s| format!("test-shard-{s} = [\"test-shard-mode\"]\n"))
                .collect::<String>()
        ),
    );
}
#[test]
fn ordered_union_package_membership_and_cli_modes() {
    let t = tempfile::tempdir().unwrap();
    cargo(t.path(), "rsid", &["session-01", "store-04"]);
    cargo(t.path(), "rsid-store", &["store-04", "store-01"]);
    let s = read_shard_packages(t.path()).unwrap();
    assert_eq!(
        s.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["store-01", "store-04", "session-01"]
    );
    assert_eq!(s[1].packages, vec!["rsid", "rsid-store"]);
    let ctx = Ctx::new(t.path(), &[]);
    let (code, out, err) = run(main, &["--shards-only", "--list-shard-packages"], &ctx);
    assert_eq!(
        (code, out, err),
        (
            0,
            "store-01 rsid-store\nstore-04 rsid rsid-store\nsession-01 rsid\n".into(),
            String::new()
        )
    );
    let (code, out, _) = run(main, &["--shards-only", "--list-shards"], &ctx);
    assert_eq!(code, 0);
    assert_eq!(out, "store-01\nstore-04\nsession-01\n");
    let (code, _, err) = run(main, &["--runtime-shard", "bad"], &ctx);
    assert_eq!(code, 2);
    assert!(err.contains("--runtime-shard requires --runtime-dir"));
    let (code, _, err) = run(
        main,
        &["--runtime-shard", "bad", "--runtime-dir", "lists"],
        &ctx,
    );
    assert_eq!(code, 1);
    assert!(err.contains("unknown runtime shard bad"));
    let f = files(&[("lib.rs", source_test("wrong", "store-01"))]);
    assert!(
        source_inventory(&f, &s, true, &Host::default())
            .unwrap_err()
            .contains("shard store-01 is not a rsid feature")
    );
}
#[test]
fn feature_manifest_failures_are_not_silently_empty() {
    let t = tempfile::tempdir().unwrap();
    for (text, error) in [
        (
            "default = [\"test-shard-mode\"]\ntest-shard-mode = []",
            "default/test-shard-mode features changed",
        ),
        (
            "default = []\ntest-shard-mode = []",
            "declares no library shard features",
        ),
        (
            "default = []\ntest-shard-mode = []\ntest-shard-bad = [\"test-shard-mode\"]",
            "invalid shard feature",
        ),
        (
            "default = []\ntest-shard-mode = []\ntest-shard-store-01 = []",
            "must imply only test-shard-mode",
        ),
    ] {
        write(
            t.path(),
            "crates/rsid/Cargo.toml",
            &format!("[features]\n{text}\n"),
        );
        assert!(
            read_packages(t.path(), &["rsid"])
                .unwrap_err()
                .contains(error)
        );
    }
}
#[test]
fn cfg_attr_exception_and_unfeatured_policy() {
    let allowed = "#![cfg_attr(\n all(test, feature = \"test-shard-mode\"),\n allow(dead_code, unused_imports)\n)]\n";
    let check = |text: &str| {
        cfg::check_shard_cfg_attributes(
            "f.rs",
            text,
            &crate::rust_source::mask(text, "f.rs").unwrap(),
        )
    };
    check(allowed).unwrap();
    for text in [
        "#![cfg_attr(feature = \"test-shard-mode\", allow(dead_code))]",
        "#[cfg_attr(all(test, feature = \"test-shard-mode\"), allow(dead_code, unused_imports))]",
        "#![cfg_attr(all(test, feature = \"test-shard-store-01\"), ignore)]",
    ] {
        assert!(
            check(text)
                .unwrap_err()
                .contains("cfg_attr must not name a test shard feature")
        );
    }
    assert!(
        check("#[cfg(feature = \"test-shard-store-01\")]\nmod hidden;")
            .unwrap_err()
            .contains("shard-only cfg hides code")
    );
    check(&source_test("a", "store-01")).unwrap();
}
#[test]
fn three_valued_host_predicates_preserve_unknown_obligations() {
    let cases = [
        ("not(unix)", [true, true, false]),
        ("all(test, not(unix))", [true, true, false]),
        ("unix", [false, false, true]),
        ("target_os = \"linux\"", [false, true, true]),
        ("not(target_os = \"macos\")", [false, true, false]),
        ("target_family = \"unix\"", [false, false, true]),
        ("any(unix, target_os = \"windows\")", [false, false, false]),
        (
            "all(test, not(any(unix, target_os = \"windows\")))",
            [true, true, true],
        ),
        (
            "all(not(unix), feature = \"optional\")",
            [true, true, false],
        ),
        (
            "any(not(unix), feature = \"optional\")",
            [false, false, false],
        ),
        ("not(feature = \"optional\")", [false, false, false]),
        (
            "not(all(unix, target_arch = \"aarch64\"))",
            [false, false, false],
        ),
        ("all(test, not(unix),)", [true, true, false]),
        ("not(unix, windows)", [false, false, false]),
        ("not(unix))", [false, false, false]),
    ];
    for (i, h) in [
        host("linux", "unix"),
        host("macos", "unix"),
        host("windows", "windows"),
    ]
    .iter()
    .enumerate()
    {
        for (predicate, expected) in cases {
            assert_eq!(
                cfg::host_excluded(&format!("#[cfg({predicate})]"), h),
                expected[i],
                "{} {predicate}",
                h.os
            );
        }
    }
}
#[test]
fn platform_item_inline_file_and_inner_exceptions_are_local() {
    let gate = |p: &str, n: &str| format!("#[cfg({p})]\n{}", source_test(n, "store-01"));
    let f = files(&[
        (
            "lib.rs",
            format!(
                "#[cfg(target_os = \"linux\")]\nmod gated_file;\n{}{}{}#[cfg(all(test, target_os = \"linux\"))]\nmod tests {{\n{}\n}}\n{}",
                gate("target_os = \"linux\"", "item_linux"),
                gate("target_os = \"macos\"", "item_macos"),
                gate("not(target_os = \"macos\")", "item_not_macos"),
                source_test("inline_linux", "store-01"),
                source_test("everywhere", "store-01")
            ),
        ),
        ("gated_file.rs", source_test("module_linux", "store-01")),
        (
            "inner.rs",
            format!(
                "#![cfg(target_os = \"linux\")]\n{}",
                source_test("inner_linux", "store-01")
            ),
        ),
    ]);
    let i = source_inventory(&f, &shards(), true, &host("macos", "unix")).unwrap();
    assert_eq!(
        i.platform_conditional["store-01"]
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "item_linux",
            "item_not_macos",
            "inline_linux",
            "module_linux",
            "inner_linux"
        ])
    );
    let native = source_inventory(&f, &shards(), true, &host("linux", "unix")).unwrap();
    assert_eq!(
        native.platform_conditional["store-01"]
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["item_macos"]
    );
}
#[test]
fn source_identity_lexing_and_gate_refusals() {
    let f = files(&[(
        "lib.rs",
        format!(
            "const S: &str = r###\"#[test]\nfn fake() {{}}\"###;\n/* nested /* #[test] */ fn fake() {{}} */\n{}",
            source_test("real", "store-01")
        ),
    )]);
    assert_eq!(inventory(&f).tests[0].name, "real");
    for (text, error) in [
        (
            format!(
                "{}{}",
                source_test("same", "store-01"),
                source_test("same", "store-01")
            ),
            "duplicate test function names",
        ),
        (
            "#[test]\nstruct NoFunction;".into(),
            "test attribute has no following function within 15 lines",
        ),
        (source_test("unknown", "store-99"), "unknown shard store-99"),
        (
            format!(
                "{}{}",
                source_test("first", "store-01"),
                source_test("second", "store-02")
            ),
            "one source file spans shards",
        ),
        (
            source_test("bad_indent", "store-01").replace("#[test]", "    #[test]"),
            "incorrect shard gate indentation",
        ),
        (
            format!(
                "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\"))]\n{}",
                source_test("duplicate", "store-01")
            ),
            "duplicate shard gates",
        ),
    ] {
        assert!(
            source_inventory(
                &files(&[("lib.rs", text)]),
                &shards(),
                true,
                &Host::default()
            )
            .unwrap_err()
            .contains(error),
            "{error}"
        );
    }
    for text in ["/* unfinished", "r##\"unfinished", "\"unfinished"] {
        assert!(extract_tests("f.rs", text).is_err());
    }
}
#[test]
fn malformed_gate_refusal_preserves_the_repair_text() {
    let gate = "any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\")";
    let target = "#[cfg(target_os = \"linux\")]";
    for text in [
        format!("#[cfg(all(target_os = \"linux\", {gate}))]\n#[test]\nfn t() {{}}\n"),
        format!("#[cfg(all(target_os = \"linux\",\n {gate}))]\n#[test]\nfn t() {{}}\n"),
        format!("#[cfg({gate})]\n{target}\n#[test]\nfn t() {{}}\n"),
    ] {
        let e = source_inventory(
            &files(&[("lib.rs", text)]),
            &shards(),
            true,
            &Host::default(),
        )
        .unwrap_err();
        assert!(e.contains(SHARD_GATE_FIX), "{e}");
    }
    assert_eq!(
        inventory(&files(&[(
            "lib.rs",
            format!("{target}\n{}", source_test("separate", "store-01"))
        )]))
        .gated,
        1
    );
}
#[test]
fn ungated_tests_suggest_the_existing_indented_shard() {
    let ungated = "#[test]\nfn new_test() {}\n";
    let f = files(&[("new.rs", ungated.into())]);
    let e = source_inventory(&f, &shards(), true, &Host::default()).unwrap_err();
    assert!(e.contains("new.rs:1: ungated test blocks a bounded lib harness"));
    assert!(e.contains("test-shard-other-01"));
    assert!(e.contains("use super::*"));
    let text = format!("{}{}", source_test("old", "store-02"), ungated);
    let text = format!(
        "mod t {{\n{}\n}}",
        text.lines()
            .map(|l| format!("    {l}\n"))
            .collect::<String>()
    );
    let f = files(&[("lib.rs", text)]);
    let e = source_inventory(&f, &shards(), true, &Host::default()).unwrap_err();
    assert!(e.contains("\n        #[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-02\"))]"));
    let i = source_inventory(&f, &shards(), false, &Host::default()).unwrap();
    assert_eq!(i.tests.len(), 2);
    assert!(i.tests.iter().all(|t| t.shard == "store-02"));
    assert!(
        source_inventory(
            &files(&[("new.rs", ungated.into())]),
            &shards(),
            false,
            &Host::default()
        )
        .unwrap_err()
        .contains("ungated test has no declared file shard")
    );
}
#[test]
fn helpers_follow_the_file_gate_and_enclosing_gated_modules() {
    let gate = source_test("x", "store-01")
        .lines()
        .next()
        .unwrap()
        .to_string();
    for (kind, helper) in [
        (
            "fn",
            "async fn launch_refusal(p: &Pilot) -> String { String::new() }",
        ),
        ("struct", "struct Fixture(Pilot);"),
        ("impl", "impl Fixture { fn new() {} }"),
        ("const", "const LIMIT: usize = 3;"),
    ] {
        let f = files(&[(
            "tests.rs",
            format!(
                "{gate}\nuse super::Pilot;\n\n{helper}\n{}",
                source_test("a_test", "store-01")
            ),
        )]);
        assert!(
            source_inventory(&f, &shards(), false, &Host::default())
                .unwrap_err()
                .contains(&format!("tests.rs:4: ungated {kind}"))
        );
    }
    for text in [
        format!(
            "{gate}\nuse super::Pilot;\n\n{gate}\nasync fn helper(p: &Pilot) {{}}\n{}",
            source_test("a_test", "store-01")
        ),
        format!(
            "use std::fmt;\nfn plain_helper() {{}}\n{gate}\nmod inner {{\n    use super::*;\n    fn helper() {{}}\n}}\n{}",
            source_test("a_test", "store-01")
        ),
    ] {
        assert_eq!(inventory(&files(&[("tests.rs", text)])).tests.len(), 1);
    }
}
#[test]
fn mounts_include_inline_paths_bins_and_path_mounted_roots() {
    let f=files(&[("lib.rs","pub mod store;\nmod plain;\nmod inline {\n    mod nested;\n}\ninclude!(\"spliced.rs\");\n".into()),("store/mod.rs","mod pending;\n".into()),("store/pending.rs","#[path = \"answers.rs\"]\npub mod answers;\n#[path = \"../shared/fix.rs\"]\nmod fix;\n".into()),("store/answers.rs","#[cfg(test)]\nmod tests;\n#[path = \"answers/more.rs\"]\nmod more;\n".into()),("store/tests.rs",source_test("beside","store-01")),("store/answers/tests.rs",source_test("unmounted","store-01")),("store/answers/more.rs","mod deep;\n".into()),("store/answers/deep.rs",source_test("path_child","store-01")),("store/deep.rs","// not mounted\n".into()),("shared/fix.rs","mod leaf;\n".into()),("shared/leaf.rs",source_test("leaf","store-01")),("plain.rs","mod child;\n".into()),("plain/child.rs",source_test("plain_child","store-01")),("inline/nested.rs",source_test("inline_child","store-01")),("spliced.rs",source_test("spliced","store-01")),("bin/tool/main.rs","mod helper;\nfn main() {}\n".into()),("bin/tool/helper.rs","#[test]\nfn bin_child() {}\n".into()),("bin/single.rs","#[test]\nfn single() {}\n".into())]);
    assert_eq!(
        unmounted_test_sources(&f).unwrap(),
        vec!["crates/rsid/src/store/answers/tests.rs"]
    );
    let mounted = mounted_sources(&f, "crates/rsid/src/lib.rs");
    for p in [
        "store/tests.rs",
        "store/answers/deep.rs",
        "shared/leaf.rs",
        "plain/child.rs",
        "inline/nested.rs",
        "spliced.rs",
    ] {
        assert!(mounted.contains(&format!("crates/rsid/src/{p}")), "{p}");
    }
    let mut fixed = f.clone();
    fixed
        .get_mut("crates/rsid/src/store/answers.rs")
        .unwrap()
        .insert_str(0, "#[path = \"answers/tests.rs\"]\n");
    // Keep cfg adjacent to mod and put path among the same attribute block.
    fixed.insert("crates/rsid/src/store/answers.rs".into(),"#[cfg(test)]\n#[path = \"answers/tests.rs\"]\nmod tests;\n#[path = \"answers/more.rs\"]\nmod more;\n".into());
    fixed.remove("crates/rsid/src/store/tests.rs");
    assert!(unmounted_test_sources(&fixed).unwrap().is_empty());
    let i = inventory(&f);
    assert!(
        i.tests
            .iter()
            .filter(|t| t.source.contains("/bin/"))
            .all(|t| t.shard == "bin")
    );
    assert!(
        source_inventory(
            &files(&[("bin/single.rs", source_test("bad", "store-01"))]),
            &shards(),
            true,
            &Host::default()
        )
        .unwrap_err()
        .contains("binary test has a library shard gate")
    );
}
#[test]
fn feature_mounts_cannot_hide_behind_runtime_exceptions() {
    let mut f = files(&[
        (
            "lib.rs",
            "#[cfg(feature = \"dev-fixtures\")]\npub mod gated;\n".into(),
        ),
        ("gated.rs", source_test("fixture_test", "store-01")),
    ]);
    let i = inventory(&f);
    assert!(
        check_shard_module_mounts(&f, &i)
            .unwrap_err()
            .contains("runtime exceptions cannot waive this")
    );
    f.insert("crates/rsid/src/lib.rs".into(),"#[cfg(any(feature = \"dev-fixtures\", all(test, feature = \"test-shard-store-01\")))]\npub mod gated;\n".into());
    check_shard_module_mounts(&f, &i).unwrap();
    for (mount, extra) in [
        (
            "#[cfg(feature = \"opt-in\")]\nmod wrapper {\n    #[path = \"../gated.rs\"]\n    mod gated;\n}\n",
            None,
        ),
        (
            "#[cfg(\n all(test, feature = \"opt-in\")\n)]\n#[path = \"gated.rs\"]\nmod named;\n",
            None,
        ),
        (
            "#[cfg(feature = \"opt-in\")]\ninclude!(\"gated.rs\");\n",
            None,
        ),
        (
            "mod parent;\n",
            Some((
                "parent.rs",
                "#[cfg(feature = \"opt-in\")]\n#[path = \"gated.rs\"]\nmod child;\n",
            )),
        ),
        (
            "mod gated;\n",
            Some(("gated.rs", "#![cfg(feature = \"opt-in\")]\n")),
        ),
    ] {
        let mut f = files(&[
            ("lib.rs", mount.into()),
            ("gated.rs", source_test("fixture_test", "store-01")),
        ]);
        if let Some((path, text)) = extra {
            let text = if path == "gated.rs" {
                format!("{text}{}", source_test("fixture_test", "store-01"))
            } else {
                text.into()
            };
            f.insert(format!("crates/rsid/src/{path}"), text);
        }
        assert!(
            check_shard_module_mounts(&f, &inventory(&f))
                .unwrap_err()
                .contains("enclosing module cfg"),
            "{mount}"
        );
    }
    let f = files(&[(
        "lib.rs",
        format!(
            "#[cfg(feature = \"opt-in\")]\nmod inline {{\n{}\n}}",
            source_test("fixture_test", "store-01")
        ),
    )]);
    assert!(check_shard_module_mounts(&f, &inventory(&f)).is_err());
}
#[test]
fn platform_and_disabled_mounts_are_not_feature_mismatches() {
    for gate in [
        "#[cfg(target_os = \"macos\")]",
        "#[cfg(not(unix))]",
        "#[cfg(any())]",
    ] {
        let f = files(&[
            ("lib.rs", format!("{gate}\nmod gated;\n")),
            ("gated.rs", source_test("fixture_test", "store-01")),
        ]);
        check_shard_module_mounts(&f, &inventory(&f)).unwrap();
    }
    let f = files(&[
        (
            "lib.rs",
            format!(
                "{}\nmod gated;\n",
                source_test("unused", "store-02").lines().next().unwrap()
            ),
        ),
        ("gated.rs", source_test("fixture_test", "store-01")),
    ]);
    assert!(check_shard_module_mounts(&f, &inventory(&f)).is_err());
}
fn suite(binary: &str, names: &[&str]) -> Value {
    json!({"binary-id":binary,"testcases":names.iter().map(|n|(format!("tests::{n}"),json!({"ignored":false,"filter-match":{"status":"matches"}}))).collect::<serde_json::Map<_,_>>()})
}
fn runtime(root: &Path, shard: &str, data: &Value) {
    write(root, &format!("{shard}.json"), &data.to_string());
}
#[test]
fn runtime_requires_exact_packages_and_source_identities() {
    let t = tempfile::tempdir().unwrap();
    let mut f = files(&[("lib.rs", source_test("remote_one", "store-01"))]);
    f.insert(
        "crates/rsid-store/src/lib.rs".into(),
        source_test("store_one", "store-01"),
    );
    let s = vec![Shard {
        name: "store-01".into(),
        packages: vec!["rsid".into(), "rsid-store".into()],
    }];
    let i = source_inventory(&f, &s, true, &Host::default()).unwrap();
    let e = RuntimeExceptions(BTreeMap::new());
    let selected = vec!["store-01".into()];
    let mut data = json!({"rust-suites":{"rsid":suite("rsid",&["remote_one"]),"rsid-store":suite("rsid-store",&["store_one"])}});
    runtime(t.path(), "store-01", &data);
    assert!(
        check_runtime(t.path(), &i, &s, &selected, &e)
            .unwrap()
            .contains("runtime union: 2 distinct")
    );
    data["rust-suites"]
        .as_object_mut()
        .unwrap()
        .remove("rsid-store");
    runtime(t.path(), "store-01", &data);
    assert!(
        check_runtime(t.path(), &i, &s, &selected, &e)
            .unwrap_err()
            .contains("expected one library suite for each")
    );
    data = json!({"rust-suites":{"rsid":suite("rsid",&["store_one"]),"rsid-store":suite("rsid-store",&["remote_one"])}});
    runtime(t.path(), "store-01", &data);
    assert!(
        check_runtime(t.path(), &i, &s, &selected, &e)
            .unwrap_err()
            .contains("runtime/static test identities differ")
    );
}
#[test]
fn runtime_ignored_cases_conditional_counts_and_no_global_leak() {
    let t = tempfile::tempdir().unwrap();
    let f = files(&[(
        "lib.rs",
        format!(
            "#[cfg(not(unix))]\n{}#[cfg(any(not(unix), feature = \"optional\"))]\n{}{}",
            source_test("unsupported", "store-01"),
            source_test("unknown_feature", "store-01"),
            source_test("everywhere", "store-01")
        ),
    )]);
    let i = source_inventory(&f, &shards(), true, &host("linux", "unix")).unwrap();
    let e = RuntimeExceptions(BTreeMap::new());
    let selected = vec!["store-01".into()];
    let mut data = json!({"rust-suites":{"rsid":suite("rsid",&["unknown_feature","everywhere"])}});
    data["rust-suites"]["rsid"]["testcases"]["tests::everywhere"] =
        json!({"ignored":true,"filter-match":{"status":"mismatch","reason":"ignored"}});
    runtime(t.path(), "store-01", &data);
    assert!(
        check_runtime(t.path(), &i, &shards(), &selected, &e)
            .unwrap()
            .contains("1 conditional source tests absent")
    );
    for required in ["everywhere", "unknown_feature"] {
        let mut missing = data.clone();
        missing["rust-suites"]["rsid"]["testcases"]
            .as_object_mut()
            .unwrap()
            .remove(&format!("tests::{required}"));
        runtime(t.path(), "store-01", &missing);
        assert!(
            check_runtime(t.path(), &i, &shards(), &selected, &e)
                .unwrap_err()
                .contains("runtime/static test identities differ")
        );
    }
    let native = source_inventory(&f, &shards(), true, &host("windows", "windows")).unwrap();
    assert!(native.platform_conditional.is_empty());
    runtime(t.path(), "store-01", &data);
    assert!(check_runtime(t.path(), &native, &shards(), &selected, &e).is_err());
    let mut malformed = data.clone();
    malformed["rust-suites"]["rsid"]["binary-id"] = json!("wrong");
    runtime(t.path(), "store-01", &malformed);
    assert!(
        check_runtime(t.path(), &i, &shards(), &selected, &e)
            .unwrap_err()
            .contains("malformed Nextest library suite")
    );
    for metadata in [
        json!({"ignored":false,"filter-match":{"status":"mismatch","reason":"ignored"}}),
        json!({"ignored":"false","filter-match":{"status":"matches"}}),
    ] {
        let mut d = data.clone();
        d["rust-suites"]["rsid"]["testcases"]["tests::everywhere"] = metadata;
        runtime(t.path(), "store-01", &d);
        assert!(
            check_runtime(t.path(), &i, &shards(), &selected, &e)
                .unwrap_err()
                .contains("filtered or malformed test")
        );
    }
    let exceptions = RuntimeExceptions(BTreeMap::from([(
        "store-01".into(),
        BTreeMap::from([("absent".into(), 1)]),
    )]));
    runtime(t.path(), "store-01", &data);
    assert!(
        check_runtime(t.path(), &i, &shards(), &selected, &exceptions)
            .unwrap_err()
            .contains("conditional runtime exception is absent from the static manifest")
    );
}
#[test]
fn runtime_new_source_tests_and_duplicate_shard_identities_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let f = files(&[
        ("first.rs", source_test("first", "store-01")),
        ("second.rs", source_test("first", "store-02")),
    ]);
    let i = inventory(&f);
    let e = RuntimeExceptions(BTreeMap::new());
    for s in ["store-01", "store-02"] {
        runtime(
            t.path(),
            s,
            &json!({"rust-suites":{"rsid":suite("rsid",&["first"])}}),
        );
    }
    assert!(
        check_runtime(
            t.path(),
            &i,
            &shards(),
            &["store-01".into(), "store-02".into()],
            &e
        )
        .unwrap_err()
        .contains("runtime test appears in multiple shards")
    );
    let f = files(&[(
        "first.rs",
        format!(
            "{}{}",
            source_test("first", "store-01"),
            source_test("new", "store-01")
        ),
    )]);
    assert!(
        check_runtime(
            t.path(),
            &inventory(&f),
            &shards(),
            &["store-01".into()],
            &e
        )
        .unwrap_err()
        .contains("runtime/static test identities differ")
    );
}
#[test]
fn static_cli_validates_mounts_and_declared_population() {
    let t = tempfile::tempdir().unwrap();
    cargo(t.path(), "rsid", &["store-01"]);
    cargo(t.path(), "rsid-store", &["store-01"]);
    for p in PACKAGES {
        write(
            t.path(),
            &format!("crates/{p}/src/lib.rs"),
            &source_test(&p.replace('-', "_"), "store-01"),
        );
    }
    let ctx = Ctx::new(t.path(), &[]);
    let (code, out, err) = run(main, &["--require-gates"], &ctx);
    assert_eq!((code, err), (0, String::new()));
    assert_eq!(
        out,
        "static inventory: 2 test identities (2 library, 0 binary); 2/2 library gates present\n"
    );
    write(
        t.path(),
        "crates/rsid/src/unmounted.rs",
        &source_test("orphan", "store-01"),
    );
    let (code, _, err) = run(main, &["--list-shards"], &ctx);
    assert_eq!(code, 1);
    assert!(err.contains("rustc never compiles them"));
    fs::remove_file(t.path().join("crates/rsid/src/unmounted.rs")).unwrap();
    write(t.path(), "crates/rsid-store/src/lib.rs", "");
    let (code, _, err) = run(main, &[], &ctx);
    assert_eq!(code, 1);
    assert!(err.contains("rsid-store declares shard store-01 but has no test in it"));
}
#[test]
fn current_tree_authoritative_static_inventory() {
    let root = repo_root();
    let s = read_shard_packages(&root).unwrap();
    let f = read_sources(&root).unwrap();
    let i = source_inventory(&f, &s, true, &Host::default()).unwrap();
    validate_inventory(&f, &i, &s).unwrap();
    assert!(i.tests.len() > 1000);
    assert!(i.gated > 1000);
}

#[test]
fn independent_branch_tests_merge_without_a_manifest_file() {
    use crate::testutil::{git, scratch_repository};
    let temp = scratch_repository();
    let root = temp.path();
    cargo(root, "rsid", &["store-01", "store-02"]);
    git(root, &["init", "-q"]);
    git(root, &["add", "crates/rsid/Cargo.toml"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["branch", "left"]);
    git(root, &["branch", "right"]);
    git(root, &["checkout", "-q", "left"]);
    write(
        root,
        "crates/rsid/src/store/left.rs",
        &source_test("left_test", "store-01"),
    );
    git(root, &["add", "crates/rsid/src/store/left.rs"]);
    git(root, &["commit", "-qm", "left test"]);
    git(root, &["checkout", "-q", "right"]);
    write(
        root,
        "crates/rsid/src/store/right.rs",
        &source_test("right_test", "store-01"),
    );
    git(root, &["add", "crates/rsid/src/store/right.rs"]);
    git(root, &["commit", "-qm", "right test"]);
    git(root, &["merge", "--no-edit", "left"]);
    let shards = read_packages(root, &["rsid"]).unwrap();
    assert_eq!(
        shards.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["store-01", "store-02"]
    );
    let i = source_inventory(
        &read_sources(root).unwrap(),
        &shards,
        true,
        &Host::default(),
    )
    .unwrap();
    assert_eq!(i.gated, 2);
    assert_eq!(
        i.tests
            .iter()
            .map(|t| t.name.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["left_test", "right_test"])
    );
    assert!(i.tests.iter().all(|t| t.shard == "store-01"));
    assert_eq!(
        fs::read_dir(root.join("crates/rsid/src/store"))
            .unwrap()
            .count(),
        2
    );
}
#[test]
fn lexical_mask_preserves_offsets_and_shards_only_does_not_scan_sources() {
    let source =
        "let a = 1; // c\nlet s = \"x//y\"; let r = r#\"q\"#; let c = '/'; /* b */ fn f() {}\n";
    let masked = crate::rust_source::mask(source, "m.rs").unwrap();
    assert_eq!(masked.len(), source.len());
    assert_eq!(masked.matches('\n').count(), source.matches('\n').count());
    assert!(masked.contains("fn f() {}"));
    let t = tempfile::tempdir().unwrap();
    for p in PACKAGES {
        cargo(t.path(), p, &["store-01"]);
    }
    write(
        t.path(),
        "crates/rsid/src/bad.rs",
        "#[test]\nfn ungated() {}\n",
    );
    let (code, out, err) = run(
        main,
        &["--shards-only", "--list-shard-packages"],
        &Ctx::new(t.path(), &[]),
    );
    assert_eq!(
        (code, out, err),
        (0, "store-01 rsid rsid-store\n".into(), String::new())
    );
}

#[test]
fn shard_numeric_order_has_no_machine_integer_limit_and_ties_are_stable() {
    let t = tempfile::tempdir().unwrap();
    cargo(
        t.path(),
        "rsid",
        &[
            "zzz-2",
            "aaa-02",
            "store-100000000000000000000000000000000000000",
            "store-99999999999999999999999999999999999999",
            "store-01",
            "store-1",
        ],
    );
    let shards = read_packages(t.path(), &["rsid"]).unwrap();
    assert_eq!(
        shards.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec![
            "store-01",
            "store-1",
            "store-99999999999999999999999999999999999999",
            "store-100000000000000000000000000000000000000",
            "zzz-2",
            "aaa-02"
        ]
    );
}

#[test]
fn metadata_target_directory_reads_the_cargo_metadata_field() {
    assert_eq!(
        metadata_target_directory(r#"{"packages":[],"target_directory":"/tmp/a b\\c"}"#).unwrap(),
        "/tmp/a b\\c"
    );
    for bad in [
        "",
        "{",
        "{}",
        r#"{"target_directory":""}"#,
        r#"{"target_directory":3}"#,
    ] {
        assert!(metadata_target_directory(bad).is_err(), "{bad}");
    }
}
