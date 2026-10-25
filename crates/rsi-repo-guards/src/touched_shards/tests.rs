//! Ported intent fixtures from test_check_touched_shards.py. No legacy program
//! is executed; expected results state ownership/coverage, never a Python oracle.
use super::*;
use filters::*;
use git::*;
use source::*;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn texts(items: &[(&str, &str)]) -> Texts {
    items
        .iter()
        .map(|(p, t)| ((*p).into(), (*t).into()))
        .collect()
}
fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).into()).collect()
}
fn set(items: &[&str]) -> BTreeSet<String> {
    strings(items).into_iter().collect()
}
fn gate(shard: &str, name: &str) -> String {
    format!(
        "#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-{shard}\"))]\n#[test]\nfn {name}() {{}}\n"
    )
}
fn observer(module: &str, name: &str, shard: &str) -> Observer {
    Observer {
        module: module.into(),
        name: name.into(),
        shard: shard.into(),
    }
}

pub(super) struct Fixture(tempfile::TempDir);
impl Fixture {
    pub(super) fn new() -> Self {
        // Fixture Git never operates in an RSI sandbox, even when TMPDIR is
        // stamped inside target/.rsi-tmp. Use the OS disk temp location on Unix.
        #[cfg(unix)]
        let root = std::path::PathBuf::from("/tmp");
        #[cfg(not(unix))]
        let root = std::env::temp_dir();
        let dir = tempfile::Builder::new()
            .prefix("rsi-touched-rust-")
            .tempdir_in(root)
            .unwrap();
        assert!(dir.path().is_absolute());
        let fixture = Self(dir);
        fixture.git(&["init", "-q"]);
        fixture
    }
    pub(super) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(super) fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        let hooks = self.path().join("empty-hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        cmd.arg("-C")
            .arg(self.path())
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
            ])
            .arg("-c")
            .arg(format!("core.hooksPath={}", hooks.display()))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            );
        cmd
    }
    pub(super) fn git(&self, args: &[&str]) -> String {
        let result = self.command().args(args).output().unwrap();
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
    pub(super) fn write(&self, path: &str, text: &str) {
        let path = self.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    pub(super) fn tree(&self, files: &[(&str, &str)]) -> String {
        self.git(&["read-tree", "--empty"]);
        for (p, t) in files {
            self.write(p, t);
            self.git(&["add", "--", p]);
        }
        self.git(&["write-tree"])
    }
    pub(super) fn commit(&self, files: &[(&str, &str)], parents: &[&str]) -> String {
        let tree = self.tree(files);
        let mut command = self.command();
        command.args(["commit-tree", &tree]);
        for p in parents {
            command.args(["-p", p]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"fixture\n").unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
    pub(super) fn repo(&self) -> Repository {
        Repository::new(self.path()).unwrap()
    }
}

#[test]
fn source_paths_preserve_package_module_and_leaf_scope() {
    assert_eq!(
        package_for("crates/rsid-store/src/store/tests.rs").as_deref(),
        Some("rsid-store")
    );
    assert_eq!(
        module_of("crates/rsid/src/a/b/mod.rs").as_deref(),
        Some("a::b")
    );
    assert_eq!(module_of("crates/rsid/src/lib.rs"), None);
    assert!(lib_source("crates/rsid-store/src/store.rs"));
    assert!(!lib_source("crates/rsid/src/bin/tool.rs"));
    assert_eq!(
        leaf_package("crates/rsi-graph/src/planning/topology.rs").as_deref(),
        Some("rsi-graph")
    );
    assert_eq!(leaf_package("crates/rsi-graph/src/render.rs"), None);
}

#[test]
fn lexical_shards_ignore_fixtures_comments_and_keep_real_gates() {
    let text = format!(
        "{}\n// feature = \"test-shard-other-99\"\n/* outer /* feature = \"test-shard-other-98\" */ end */\nlet a = r##\"feature = \"test-shard-other-97\"\"##;\nlet b = \"feature = \\\"test-shard-other-96\\\"\";\nlet c = b'\"';\n#[cfg(feature = r#\"test-shard-session-03\"#)]\n",
        gate("store-02", "real")
    );
    assert_eq!(
        code_shards(&text).unwrap(),
        set(&["store-02", "session-03"])
    );
    assert!(shards_in(&text).contains("other-99"));
}

#[test]
fn malformed_lexical_inputs_fail_closed() {
    for (text, reason) in [
        ("/* nested /* */", "unterminated Rust comment"),
        ("let s = r##\"x\"#;", "unterminated Rust raw string"),
        ("let s = \"x", "unterminated Rust string"),
    ] {
        assert_eq!(code_shards(text).unwrap_err(), reason);
    }
    assert_eq!(
        code_text("é /* 中 */ x\n\"中\" y").unwrap().lines().count(),
        2
    );
}

#[test]
fn path_mounts_includes_and_ordinary_stems_resolve_distinct_owners() {
    let files = texts(&[
        ("crates/rsid/src/lib.rs", "mod manager;\nmod regular;\n"),
        (
            "crates/rsid/src/manager.rs",
            "#[path = \"shared/actions.rs\"]\nmod issue_worker;\n",
        ),
        (
            "crates/rsid/src/shared/actions.rs",
            "mod tests;\ninclude!(\"fragment.rs\");\n",
        ),
        ("crates/rsid/src/shared/tests.rs", ""),
        ("crates/rsid/src/shared/fragment.rs", ""),
        ("crates/rsid/src/regular.rs", "mod tests;\n"),
        ("crates/rsid/src/regular/tests.rs", ""),
        ("crates/rsid/src/shared/actions/tests.rs", "unmounted"),
    ]);
    let mounts = module_map(&files, "linux");
    assert_eq!(
        mounts.modules["crates/rsid/src/shared/tests.rs"],
        "manager::issue_worker::tests"
    );
    assert_eq!(
        mounts.modules["crates/rsid/src/shared/fragment.rs"],
        "manager::issue_worker"
    );
    assert_eq!(
        mounts.modules["crates/rsid/src/regular/tests.rs"],
        "regular::tests"
    );
    let tree = SourceTree::new(files, "linux");
    assert_eq!(
        tree.child_sources("crates/rsid/src/shared/actions.rs"),
        strings(&[
            "crates/rsid/src/shared/actions.rs",
            "crates/rsid/src/shared/tests.rs"
        ])
    );
}

#[test]
fn inline_path_mount_and_host_reachability_follow_every_chain() {
    let files = texts(&[
        (
            "crates/rsid/src/lib.rs",
            "#[cfg(target_os = \"macos\")]\nmod foreign {\n    #[path = \"../../shared.rs\"]\n    mod tests;\n}\n#[path = \"shared.rs\"] mod live;\n",
        ),
        ("crates/rsid/src/shared.rs", "mod child;\n"),
        ("crates/rsid/src/child.rs", ""),
    ]);
    let mounts = module_map(&files, "linux");
    assert!(mounts.modules.contains_key("crates/rsid/src/shared.rs"));
    assert!(mounts.gated_off.is_empty());
    let only_off = texts(&[
        (
            "crates/a/src/lib.rs",
            "#[cfg(target_os = \"macos\")]\nmod x;\n",
        ),
        ("crates/a/src/x.rs", "mod child;\n"),
        ("crates/a/src/x/child.rs", ""),
    ]);
    assert_eq!(
        module_map(&only_off, "linux").gated_off,
        set(&["crates/a/src/x.rs", "crates/a/src/x/child.rs"])
    );
}

#[test]
fn library_root_wins_and_platform_subset_keeps_unknown_predicates() {
    let files = texts(&[
        (
            "crates/a/src/lib.rs",
            "#[path = \"shared.rs\"] mod library;\n",
        ),
        (
            "crates/a/src/main.rs",
            "#[path = \"shared.rs\"] mod binary;\n",
        ),
        ("crates/a/src/shared.rs", ""),
    ]);
    assert_eq!(
        module_map(&files, "linux").modules["crates/a/src/shared.rs"],
        "library"
    );
    assert!(other_os_gate("#[cfg(not(target_os = \"linux\"))]", "linux"));
    assert!(other_os_gate(
        "#[cfg(all(target_os = \"macos\", feature = \"x\"))]",
        "linux"
    ));
    assert!(!other_os_gate("#[cfg(feature = \"x\")]", "linux"));
    assert!(file_gated_off(
        "#![cfg(target_os = \"windows\")]\nfn t() {}",
        "linux"
    ));
}

#[test]
fn shard_test_inventory_counts_only_immediately_gated_tests() {
    let text = format!(
        "#[cfg(feature = \"test-shard-other-03\")]\nfn helper() {{}}\n{}{}",
        gate("store-01", "a"),
        gate("store-02", "b")
    );
    let counts = test_shard_counts(&text);
    assert_eq!(
        counts,
        BTreeMap::from([("store-01".into(), 1), ("store-02".into(), 1)])
    );
    assert_eq!(
        code_shards(&text).unwrap(),
        set(&["other-03", "store-01", "store-02"])
    );
}

#[test]
fn target_owners_follow_external_mounts_and_cargo_layout() {
    let tree = SourceTree::new(
        texts(&[
            ("crates/a/src/lib.rs", ""),
            (
                "crates/a/src/bin/tool.rs",
                "#[path = \"../../shared.rs\"] mod shared;\n",
            ),
            ("crates/a/shared.rs", "#[test]\nfn runs() {}\n"),
            ("crates/a/tests/probe/main.rs", "mod helper;\n"),
            ("crates/a/tests/probe/helper.rs", "#[test]\nfn runs() {}\n"),
        ]),
        "linux",
    );
    assert_eq!(tree.target_owners("crates/a/shared.rs")[0].name, "tool");
    let target = tree
        .target_owners("crates/a/tests/probe/helper.rs")
        .remove(0);
    assert_eq!(
        (target.kind.as_str(), target.name.as_str()),
        ("test", "probe")
    );
    assert!(tree.target_declares_tests(&target).unwrap());
}

#[test]
fn runnable_inventory_handles_ignored_platform_and_unknown_tests() {
    assert!(!holds_runnable_test("// #[test]\nlet x = \"#[test]\";", "linux").unwrap());
    assert!(!holds_runnable_test("#[test]\n#[ignore]\nfn inert() {}\n", "linux").unwrap());
    assert!(
        !holds_runnable_test(
            "#[cfg(target_os = \"macos\")]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
            "linux"
        )
        .unwrap()
    );
    assert!(holds_runnable_test("#[test_case(1)]\nfn generated() {}\n", "linux").unwrap());
    // Unknown predicates keep the test unless the complete condition is provably false.
    assert!(
        holds_runnable_test(
            "#[cfg(all(target_os = \"linux\", unknown))]\n#[test]\nfn t() {}\n",
            "linux"
        )
        .unwrap()
    );
    assert!(
        !holds_runnable_test(
            "#[cfg(all(target_os = \"macos\", unknown))]\n#[test]\nfn t() {}\n",
            "linux"
        )
        .unwrap()
    );
}

#[test]
fn feature_inventory_names_nested_tests_and_retains_required_feature() {
    let tree = SourceTree::new(
        texts(&[(
            "crates/rsid/src/recursive_dag/smoke_fixture.rs",
            "#[test]\nfn root() {}\nmod nested {\n    #[tokio::test]\n    async fn inside() {}\n}\n",
        )]),
        "linux",
    );
    assert_eq!(
        tree.feature_test_inventory("rsid").unwrap(),
        vec![
            (
                "recursive_dag::smoke_fixture::root".into(),
                strings(&["dev-fixtures"])
            ),
            (
                "recursive_dag::smoke_fixture::nested::inside".into(),
                strings(&["dev-fixtures"])
            )
        ]
    );
    assert_eq!(tree.feature_test_inventory("rsi").unwrap().len(), 0);
}

#[test]
fn changed_module_records_keep_owned_tests_and_exclude_unchanged_production_children() {
    let tests = gate("session-01", "owns");
    let tree = SourceTree::new(
        texts(&[
            ("crates/rsid/src/lib.rs", "mod session;\n"),
            ("crates/rsid/src/session.rs", "mod worker;\nmod tests;\n"),
            ("crates/rsid/src/session/worker.rs", ""),
            ("crates/rsid/src/session/tests.rs", &tests),
        ]),
        "linux",
    );
    let result = tree.changed_modules(&set(&["crates/rsid/src/session.rs"]), &BTreeSet::new());
    assert_eq!(result[0]["module"], "session");
    assert_eq!(
        result[0]["excluded"],
        serde_json::json!(["session::worker"])
    );
    assert_eq!(
        tree.changed_modules(
            &set(&["crates/rsid/src/session.rs"]),
            &set(&["crates/rsid/src/session.rs"])
        )
        .len(),
        0
    );
}

#[test]
fn store_fallback_uses_store_owner_and_adds_daemon_compile_closure() {
    let path = "crates/rsid-store/src/store.rs";
    let tree = SourceTree::new(texts(&[(path, "pub fn store() {}")]), "linux");
    let inventory = vec![
        shard("other-01", &["rsid"]),
        shard("store-01", &["rsid-store"]),
    ];
    let result = map_shards(&tree, &strings(&[path]), &inventory).unwrap();
    assert_eq!(result.fallback.as_deref(), Some("store-01"));
    assert_eq!(result.map["store-01"], strings(&[path]));
    assert_eq!(result.map["other-01"], Vec::<String>::new());
}

#[test]
fn missing_or_inconsistent_inventory_is_refused_without_guessing_shards() {
    let tree = SourceTree::new(
        texts(&[("crates/rsid/src/a.rs", &gate("other-02", "a"))]),
        "linux",
    );
    assert!(map_shards(&tree, &[], &[]).unwrap_err().contains("#362"));
    assert!(
        map_shards(
            &tree,
            &strings(&["crates/rsid/src/a.rs"]),
            &[shard("other-01", &["rsid"])]
        )
        .unwrap_err()
        .contains("undeclared shard: other-02")
    );
}

#[test]
fn named_chunks_preserve_every_observer_within_wire_limit() {
    let tests: Vec<_> = (0..30)
        .map(|i| {
            observer(
                "rpc::tests",
                &format!("catalog_observer_with_long_identity_{i:02}"),
                "other-01",
            )
        })
        .collect();
    let filters = traced_filters(&tests);
    assert!(filters.len() > 1);
    assert!(filters.iter().all(|f| f.len() <= GATE_FILTER_LENGTH));
    for test in tests {
        assert_eq!(filters.iter().filter(|f| f.contains(&test.name)).count(), 1);
    }
    let huge = traced_filters(&[observer("rpc::tests", &"x".repeat(300), "other-01")]);
    assert_eq!(huge.len(), 1);
    assert!(huge[0].len() > GATE_FILTER_LENGTH);
}

#[test]
fn coverage_merging_preserves_shards_and_proves_only_syntactic_containment() {
    let input = strings(&[
        "rsid=shard:other-01:test(/word/)",
        "rsid=shard:other-01:test(/^rpc::tests::a$/)",
        "rsid=shard:other-01:test(rpc)",
        "rsid=shard:other-02:test(/^rpc::tests::a$/)",
    ]);
    let mut merged = Vec::new();
    let result = covered_filters(&input, &mut merged);
    assert_eq!(result, strings(&[&input[0], &input[2], &input[3]]));
    assert_eq!(merged[0].covered_by, input[2]);
    assert_eq!(
        covered_filters(
            &strings(&[
                "rsid=shard:other-01:test(/^rpc::/)",
                "rsid=shard:other-01:test(/^rpc2::x$/)"
            ]),
            &mut Vec::new()
        )
        .len(),
        2
    );
}

#[test]
fn identical_unsharded_expressions_keep_distinct_package_coverage() {
    let inventory = BTreeMap::from([
        ("store-01".into(), strings(&["rsid-store"])),
        ("store-02".into(), strings(&["rsid-store"])),
        ("other-01".into(), strings(&["rsid"])),
    ]);
    let values = strings(&[
        "rsid=shard:store-01:test(store)",
        "rsid=shard:store-02:test(store)",
        "rsid=shard:other-01:test(store)",
    ]);
    assert_eq!(
        unsharded_filters(&values, &inventory, &mut Vec::new()),
        vec![values[0].clone(), values[2].clone()]
    );
}

#[test]
fn direct_observer_ranks_use_nearest_identity_and_distance_order() {
    let found = BTreeMap::from([
        (observer("a::tests", "near", "store-01"), 1),
        (observer("a::tests", "near", "store-02"), 3),
        (observer("a::tests", "far", "store-01"), 4),
    ]);
    assert_eq!(
        direct_observer_filters(&found)
            .iter()
            .map(|f| f.distance)
            .collect::<Vec<_>>(),
        vec![1, 3, 4]
    );
    assert_eq!(
        direct_observer_summary(&found),
        serde_json::json!({"tests":2,"by_distance":{"1":1,"4":1}})
    );
}

#[test]
fn slow_config_validates_names_types_duplicates_and_generated_filter() {
    let config = slow::parse_slow_tier(
        "package = 'rsid-store'\ntrigger_paths = ['store/']\ntests = ['store::b','store::a']",
    )
    .unwrap();
    assert_eq!(
        slow::slow_tier_filter(&config),
        "not (package(rsid-store) & test(/^(?:store::a|store::b)$/))"
    );
    for (text, error) in [
        ("package = 1\ntrigger_paths=[]\ntests=[]", "package must be"),
        ("package='a'\ntrigger_paths=1\ntests=[]", "trigger_paths"),
        ("package='a'\ntrigger_paths=[]\ntests=['a*']", "test names"),
        ("package='a'\ntrigger_paths=[]\ntests=['a','a']", "twice"),
    ] {
        assert!(slow::parse_slow_tier(text).unwrap_err().contains(error));
    }
}

#[test]
fn slow_decision_distinguishes_fast_test_body_from_shared_helper() {
    let text = "fn helper() {}\n#[test]\nfn slow() {\n    helper();\n}\n#[test]\nfn fast() {\n    helper();\n}\n";
    let names = set(&["slow"]);
    assert_eq!(
        slow::slow_test_source_change(Some(text), Some(text), "@@ -8 +8 @@", &names),
        None
    );
    assert_eq!(
        slow::slow_test_source_change(Some(text), Some(text), "@@ -1 +1 @@", &names),
        Some(1)
    );
    assert_eq!(
        slow::slow_test_source_change(Some(text), Some(text), "@@ -4 +4 @@", &names),
        Some(4)
    );
    assert_eq!(
        slow::slow_test_source_change(Some(text), None, "", &names),
        Some(1)
    );
}

#[test]
fn slow_spans_accept_multiline_attributes_and_refuse_broken_layout() {
    let text = "#[test_case(\n    1,\n)]\nfn fast() {\n    run();\n}\n";
    assert_eq!(
        slow::test_fn_spans(text).unwrap()[0],
        slow::TestSpan {
            first: 1,
            last: 6,
            name: "fast".into()
        }
    );
    assert_eq!(
        slow::test_fn_spans("#[test]\nfn broken() {\nfn other() {}\n"),
        None
    );
    assert_eq!(
        changed_line_ranges("@@ -0,0 +1,2 @@ x\n@@ -8 +10,0 @@\n"),
        vec![
            Hunk {
                old_first: 0,
                old_count: 0,
                new_first: 1,
                new_count: 2
            },
            Hunk {
                old_first: 8,
                old_count: 1,
                new_first: 10,
                new_count: 0
            }
        ]
    );
}

#[test]
fn workspace_graph_includes_inherited_renamed_target_dev_and_build_edges() {
    let data = texts(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers=['crates/*']\nexclude=['crates/excluded']\n[workspace.dependencies]\nrenamed={package='base',path='crates/base'}\n",
        ),
        ("crates/base/Cargo.toml", "[package]\nname='base'\n"),
        ("crates/base/src/lib.rs", "pub fn api() {}"),
        (
            "crates/consumer/Cargo.toml",
            "[package]\nname='consumer'\n[dependencies]\nrenamed={workspace=true}\n[target.'cfg(windows)'.dev-dependencies]\nplatform={path='../platform'}\n[build-dependencies]\nbuild={path='../builder'}\n",
        ),
        ("crates/consumer/src/lib.rs", ""),
        ("crates/platform/Cargo.toml", "[package]\nname='platform'\n"),
        ("crates/builder/Cargo.toml", "[package]\nname='builder'\n"),
        ("crates/excluded/Cargo.toml", "this is not parsed"),
    ]);
    let packages =
        downstream::workspace_test_graph(&data, &data.keys().cloned().collect()).unwrap();
    assert_eq!(
        packages["consumer"].dependencies,
        set(&["base", "platform", "builder"])
    );
    assert_eq!(packages.len(), 4);
    assert_eq!(
        packages["base"].library.as_deref(),
        Some("crates/base/src/lib.rs")
    );
}

#[test]
fn public_api_classification_excludes_proven_private_and_test_edits() {
    let text = "pub struct Public;\nfn private() {\n    step();\n}\npub fn exported() {\n    step();\n}\n#[test]\nfn test() {\n    step();\n}\n";
    assert!(!downstream::public_api_edit(text, &BTreeSet::from([3]), "linux").unwrap());
    assert!(!downstream::public_api_edit(text, &BTreeSet::from([10]), "linux").unwrap());
    assert!(downstream::public_api_edit(text, &BTreeSet::from([6]), "linux").unwrap());
    assert!(downstream::public_api_edit(text, &BTreeSet::from([1]), "linux").unwrap());
}

#[test]
fn reverse_dependency_closure_retains_transitive_observers_and_cycles() {
    let edges = BTreeMap::from([
        ("base".into(), set(&["cycle"])),
        ("middle".into(), set(&["base"])),
        ("top".into(), set(&["middle"])),
        ("cycle".into(), set(&["base"])),
        ("unrelated".into(), set(&[])),
    ]);
    assert_eq!(
        downstream::reverse_closure(&edges, &set(&["base"])),
        set(&["base", "middle", "top", "cycle"])
    );
}

#[test]
fn git_blob_batch_uses_pinned_objects_and_drains_large_pipes() {
    let fixture = Fixture::new();
    let large = "pub fn large() {}\n".repeat(12000);
    let head = fixture.commit(&[("a.rs", &large), ("odd name.rs", "pinned")], &[]);
    fixture.write("odd name.rs", "dirty bytes");
    let paths = strings(&["a.rs", "odd name.rs"]);
    assert_eq!(
        fixture.repo().blobs(&head, &paths).unwrap()["odd name.rs"],
        "pinned"
    );
    assert_eq!(
        fixture.repo().blobs(&head, &paths).unwrap()["a.rs"].len(),
        large.len()
    );
    assert!(fixture.repo().blobs(&head, &strings(&["missing"])).is_err());
}

#[test]
fn ordinary_diff_keeps_single_merge_base_and_added_deleted_paths() {
    let fixture = Fixture::new();
    let base = fixture.commit(&[("old.rs", "old")], &[]);
    let head = fixture.commit(&[("new.rs", "new")], &[&base]);
    let diff = DiffContext::load(&fixture.repo(), &base, &head).unwrap();
    assert_eq!(diff.merge_base, base);
    assert_eq!(diff.merge_bases, vec![base]);
    assert_eq!(diff.changed, strings(&["new.rs", "old.rs"]));
    assert_eq!(diff.added, strings(&["new.rs"]));
}

#[test]
fn multiple_merge_bases_count_only_the_integrated_candidate_delta() {
    let fixture = Fixture::new();
    let base = fixture.commit(&[("a", "0"), ("b", "0")], &[]);
    let a = fixture.commit(&[("a", "1"), ("b", "0")], &[&base]);
    let b = fixture.commit(&[("a", "0"), ("b", "1")], &[&base]);
    let left = fixture.commit(&[("a", "1"), ("b", "1")], &[&a, &b]);
    let right = fixture.commit(&[("a", "1"), ("b", "1")], &[&b, &a]);
    let rolling = fixture.commit(&[("a", "2"), ("b", "2")], &[&left]);
    let head = fixture.commit(&[("a", "1"), ("b", "1"), ("notes", "candidate")], &[&right]);
    let diff = DiffContext::load(&fixture.repo(), &rolling, &head).unwrap();
    assert_eq!(diff.merge_bases.len(), 2);
    assert_eq!(diff.merge_base, rolling);
    assert_eq!(diff.merge_clean, Some(true));
    assert_eq!(diff.changed, strings(&["notes"]));
}

#[test]
fn downstream_compile_plan_unions_old_and_new_dependency_edges() {
    let fixture = Fixture::new();
    let manifest = "[workspace]\nmembers=['crates/*']\n";
    let base_manifest = "[package]\nname='base'\n";
    let depends = "[package]\nname='consumer'\n[dev-dependencies]\nbase={path='../base'}\n";
    let before = fixture.commit(
        &[
            ("Cargo.toml", manifest),
            ("crates/base/Cargo.toml", base_manifest),
            (
                "crates/base/src/lib.rs",
                "pub fn external_api() {\n    old();\n}\n",
            ),
            ("crates/consumer/Cargo.toml", depends),
            ("crates/consumer/src/lib.rs", ""),
        ],
        &[],
    );
    let after = fixture.commit(
        &[
            ("Cargo.toml", manifest),
            ("crates/base/Cargo.toml", base_manifest),
            (
                "crates/base/src/lib.rs",
                "pub fn external_api() {\n    new();\n}\n",
            ),
            ("crates/consumer/Cargo.toml", "[package]\nname='consumer'\n"),
            ("crates/consumer/src/lib.rs", ""),
        ],
        &[&before],
    );
    let result = downstream::downstream_compile_plan(
        &fixture.repo(),
        &before,
        &after,
        &strings(&["crates/base/src/lib.rs"]),
        "linux",
    )
    .unwrap();
    assert_eq!(
        result.changed_libraries,
        vec![downstream::ChangedLibrary {
            package: "base".into(),
            paths: strings(&["crates/base/src/lib.rs"])
        }]
    );
    assert_eq!(
        result.packages,
        vec![downstream::PendingPackage {
            package: "consumer".into(),
            status: "pending".into()
        }]
    );
}

#[test]
fn staged_filter_entry_refuses_every_receipt_shape_without_partial_output() {
    let dir = Fixture::new();
    let ctx = Ctx::new(dir.path(), &[]);
    for args in [
        strings(&["--filters-only", "--json"]),
        strings(&[
            "--no-compile",
            "--receipt-line",
            "--receipt",
            "receipt.json",
        ]),
        strings(&["--compile"]),
    ] {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(main(&args, &ctx, &mut out, &mut err), 2);
        assert_eq!(out, Vec::<u8>::new());
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("incomplete_contract")
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|p| p.file_name() == "receipt.json")
                .count(),
            0
        );
    }
    for args in [
        strings(&[]),
        strings(&["--compile", "--filters-only"]),
        strings(&["--filters-only", "--run-filters"]),
    ] {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(main(&args, &ctx, &mut out, &mut err), 2);
        assert!(String::from_utf8(err).unwrap().contains("error:"));
    }
}

#[test]
fn slow_tier_cli_reads_pinned_candidate_config_and_generator_is_exact() {
    let fixture = Fixture::new();
    let config = "package='rsid-store'\ntrigger_paths=['migrations/']\ntests=['store::slow']\n";
    let base = fixture.commit(&[(slow::CONFIG, config), ("notes", "a")], &[]);
    let head = fixture.commit(&[(slow::CONFIG, config), ("notes", "b")], &[&base]);
    fixture.write(slow::CONFIG, "broken dirty config");
    let ctx = Ctx::new(fixture.path(), &[]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        main(
            &strings(&["--base", &base, "--head", &head, "--slow-tier-only"]),
            &ctx,
            &mut out,
            &mut err
        ),
        0
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out).unwrap(),
        serde_json::json!({"mode":"excluded","reason":"no_trigger","tests":1})
    );
    fixture.write(slow::CONFIG, config);
    fixture.write(
        slow::NEXTEST_CONFIG,
        "[profile.default]\ndefault-filter = 'not (package(old) & all())'\n",
    );
    assert!(
        slow::write_slow_tier_filter(fixture.path())
            .unwrap()
            .contains("(1 tests)")
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path().join(slow::NEXTEST_CONFIG)).unwrap(),
        "[profile.default]\ndefault-filter = 'not (package(rsid-store) & test(/^(?:store::slow)$/))'\n"
    );
}

fn shard(name: &str, packages: &[&str]) -> crate::rsid_test_shards::Shard {
    crate::rsid_test_shards::Shard {
        name: name.into(),
        packages: strings(packages),
    }
}

#[test]
fn compound_platform_gates_match_the_runtime_inventory_for_every_host() {
    let cases: [(&str, [bool; 3]); 5] = [
        (
            "#[cfg(all(target_os = \"macos\", unix))]",
            [true, false, true],
        ),
        (
            "#[cfg(any(target_os = \"macos\", unix))]",
            [false, false, true],
        ),
        ("#[cfg(not(unix))]", [true, true, false]),
        ("#[cfg(all(test, not(unix)))]", [true, true, false]),
        (
            "#[cfg(any(not(unix), feature = \"x\"))]",
            [false, false, false],
        ),
    ];
    for (index, host) in ["linux", "macos", "windows"].into_iter().enumerate() {
        for (row, expected) in cases {
            assert_eq!(other_os_gate(row, host), expected[index], "{host} {row}");
        }
    }
}

fn function_names(text: &str, host: &str) -> Vec<String> {
    source::rust_functions(text, host)
        .into_iter()
        .map(|f| f.name)
        .collect()
}

#[test]
fn empty_any_spellings_exclude_only_their_item_and_never_hold_a_runnable_test() {
    for gate in ["#[cfg(any())]", "#[cfg(any( ))]", "#[ cfg ( any (\t) ) ]"] {
        let disabled = format!("{gate}\n#[test]\nfn disabled() {{}}\n");
        assert_eq!(
            function_names(&format!("{disabled}#[test]\nfn enabled() {{}}\n"), "linux"),
            ["enabled"]
        );
        assert!(!source::holds_runnable_test(&disabled, "linux").unwrap());
    }
}

#[test]
fn disabled_inline_module_excludes_nested_tests_and_keeps_its_neighbour() {
    let disabled = "#[cfg(any())]\nmod disabled {\n    #[test]\n    fn t() {}\n    mod nested {\n        #[test]\n        fn nested_test() {}\n    }\n}\n";
    assert_eq!(
        function_names(&format!("{disabled}#[test]\nfn enabled() {{}}\n"), "linux"),
        ["enabled"]
    );
    assert!(!source::holds_runnable_test(disabled, "linux").unwrap());
}

#[test]
fn file_or_external_module_with_empty_any_gate_is_compiled_out() {
    let test = "#[test]\nfn t() {}\n";
    assert!(function_names(&format!("#![cfg(any( ))]\n{test}"), "linux").is_empty());
    let files = texts(&[
        (
            "crates/x/src/lib.rs",
            "#[cfg(any())]\nmod disabled;\nmod enabled;\n",
        ),
        ("crates/x/src/disabled.rs", test),
        ("crates/x/src/enabled.rs", test),
    ]);
    assert_eq!(
        module_map(&files, "linux").gated_off,
        BTreeSet::from(["crates/x/src/disabled.rs".to_owned()])
    );
}

#[test]
fn other_cfg_predicates_keep_their_tests() {
    for gate in [
        "#[cfg(any(test))]",
        "#[cfg(all())]",
        "#[cfg(not(any()))]",
        "#[cfg(feature = \"x\")]",
    ] {
        assert_eq!(
            function_names(&format!("{gate}\n#[test]\nfn enabled() {{}}\n"), "linux"),
            ["enabled"],
            "{gate}"
        );
    }
}

#[test]
fn multiline_cfg_attributes_reach_the_function_they_gate() {
    let text = "#[cfg(any(\n    target_os = \"macos\",\n    target_os = \"windows\"\n))]\n#[test]\nfn off_here() {}\n#[test]\nfn kept() {}\n";
    assert_eq!(function_names(text, "linux"), ["kept"]);
    assert_eq!(function_names(text, "macos"), ["off_here", "kept"]);
    let found = source::rust_functions(text, "macos");
    assert_eq!(found[0].attr_start, 0);
}
