use super::*;
use crate::testutil::{git as fixture_git, repo_root, scratch_repository, write};
const GUARD: &str = "crates/rsi-repo-guards/src/target_reclaim.rs";
const DETECT: &str = "pub fn entries(text: &str) -> Vec<(usize, String)> { text.lines().enumerate().filter(|(_, s)| s.contains(\"FLAG\")).map(|(i, _)| (i + 1, String::from(\"flag\"))).collect() }\n";
const EMPTY: &str = "pub fn entries(_: &str) -> Vec<(usize, String)> { Vec::new() }\n";
struct Fixture {
    temp: tempfile::TempDir,
    base: String,
}
impl Fixture {
    fn new() -> Self {
        let temp = scratch_repository();
        let root = temp.path();
        fixture_git(root, &["init", "-q"]);
        write(root, GUARD, DETECT);
        write(root, "scripts/writer", "FLAG\n");
        fixture_git(root, &["add", GUARD, "scripts/writer"]);
        fixture_git(root, &["commit", "-qm", "base"]);
        let base = fixture_git(root, &["rev-parse", "HEAD"]).trim().into();
        Self { temp, base }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn commit(&self, message: &str) -> String {
        fixture_git(self.root(), &["add", "--all"]);
        fixture_git(self.root(), &["commit", "-qm", message]);
        fixture_git(self.root(), &["rev-parse", "HEAD"])
            .trim()
            .into()
    }
    fn loosen(&self) {
        write(self.root(), GUARD, EMPTY);
        self.commit("loosen");
    }
    fn losses(&self) -> Result<Comparison, String> {
        compare_with(self.root(), &self.base, "HEAD", None, &FixtureAdapters)
    }
}
struct FixtureAdapters;
impl HistoricalAdapters for FixtureAdapters {
    fn collect(
        &self,
        _guard: &str,
        version: &Blob,
        _commit: &str,
        kind: AdapterKind,
        head: &Snapshot,
        _deadline: Instant,
    ) -> Result<BTreeSet<Finding>, String> {
        // Test-only native semantics for exact source fixtures. No checker
        // source is compiled or executed; production has its frozen registry.
        if kind != AdapterKind::Entries {
            return Err("wrong fixture adapter".into());
        }
        let source = std::str::from_utf8(&version.bytes).map_err(|e| e.to_string())?;
        if !source.starts_with("pub fn entries(") {
            return Err("needs entries".into());
        }
        if source == EMPTY || source == format!("{EMPTY}// later\n") {
            return Ok(BTreeSet::new());
        }
        if source != DETECT && source != format!("{DETECT}// changed\n") {
            return Err("unauthenticated fixture source".into());
        }
        let mut findings = BTreeSet::new();
        for (path, blob) in head {
            if !source_path(path) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&blob.bytes) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if line.contains("FLAG") {
                    findings.insert(validate_finding(path, i + 1, "flag")?);
                }
            }
        }
        Ok(findings)
    }
}
#[test]
fn same_guard_and_new_guard_have_no_baseline_comparison() {
    let f = Fixture::new();
    write(f.root(), "scripts/writer", "FLAG\nFLAG\n");
    f.commit("writer");
    assert!(f.losses().unwrap().losses.is_empty());
    assert!(
        compare_with(
            f.root(),
            &f.base,
            "HEAD",
            Some(&[GUARD.into()]),
            &FixtureAdapters
        )
        .unwrap()
        .losses
        .is_empty()
    );
    write(
        f.root(),
        "tools/check-new.py",
        &std::fs::read(repo_root().join("scripts/check-target-reclaim.py"))
            .map(|b| String::from_utf8(b).unwrap())
            .unwrap(),
    );
    f.commit("new committed historical blob");
    assert!(f.losses().unwrap().losses.is_empty());
}
#[test]
fn committed_tree_ignores_dirty_worktree_and_archive_attributes() {
    let f = Fixture::new();
    write(f.root(), "scripts/writer", "heading\nFLAG\n");
    write(f.root(), ".gitattributes", "scripts/writer export-ignore\n");
    f.loosen();
    write(f.root(), "scripts/writer", "dirty\n");
    let c = f.losses().unwrap();
    assert_eq!(
        c.losses
            .iter()
            .map(|l| (
                &*l.finding.path,
                l.finding.line,
                &*l.finding.finding,
                l.intended
            ))
            .collect::<Vec<_>>(),
        vec![("scripts/writer", 2, "flag", false)]
    );
    let (code, out, err) = crate::testutil::run(
        main,
        &[&f.base, "HEAD", "--guard", GUARD],
        &Ctx::new(f.root(), &[]),
    );
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert!(err.contains("authenticated historical adapter required"));
}
#[test]
fn test_paths_and_inline_rust_items_exclude_only_test_losses() {
    let f = Fixture::new();
    for path in [
        "scripts/tests/test_writer.rs",
        "crates/x/tests/writer.rs",
        "crates/x/src/tests.rs",
        "crates/x/src/e2e_tests/writer.rs",
    ] {
        write(f.root(), path, "FLAG\n");
    }
    write(
        f.root(),
        "crates/x/src/writer.rs",
        "#[cfg(test)]\nmod tests {\n // FLAG\n fn f() { let s = r#\"}\"#; }\n}\n#[test]\nfn fixture() { /* outer /* FLAG */ nested */ }\nfn production() { /* FLAG */ }\n",
    );
    f.loosen();
    assert_eq!(
        f.losses()
            .unwrap()
            .losses
            .iter()
            .map(|l| (&*l.finding.path, l.finding.line))
            .collect::<Vec<_>>(),
        vec![("crates/x/src/writer.rs", 8), ("scripts/writer", 1)]
    );
}
#[test]
fn deletion_retains_exact_old_blob_and_null_head_identity() {
    let f = Fixture::new();
    std::fs::remove_file(f.root().join(GUARD)).unwrap();
    f.commit("delete guard");
    let c = f.losses().unwrap();
    assert_eq!(c.losses.len(), 1);
    assert_eq!(c.losses[0].head_guard, None);
    assert_eq!(
        c.losses[0].base_guard,
        blob(f.root(), &f.base, GUARD).unwrap().unwrap().oid
    );
}
#[test]
fn exact_declarations_do_not_approve_later_blobs_or_other_findings() {
    let f = Fixture::new();
    write(f.root(), "scripts/second", "FLAG\n");
    f.loosen();
    let first = f.losses().unwrap().losses[0].clone();
    let mut declaration = first.declaration();
    declaration["reason"] = json!("Reviewed fixture alias.");
    write(
        f.root(),
        DECLARATIONS,
        &json!([declaration.clone()]).to_string(),
    );
    f.commit("declare one");
    let c = f.losses().unwrap();
    assert_eq!(
        c.losses.iter().map(|l| l.intended).collect::<Vec<_>>(),
        vec![true, false]
    );
    write(f.root(), GUARD, &format!("{EMPTY}// later\n"));
    f.commit("later checker");
    assert!(f.losses().unwrap().losses.iter().all(|l| !l.intended));
    declaration["reason"] = json!(" ");
    write(f.root(), DECLARATIONS, &json!([declaration]).to_string());
    f.commit("empty reason");
    assert!(f.losses().unwrap_err().contains("each exact loss needs"));
}
#[test]
fn unsupported_checkers_do_not_hide_supported_losses_or_bad_declarations() {
    let mut f = Fixture::new();
    let path = "tools/check-unsupported.py";
    let spec = std::fs::read(repo_root().join("scripts/check-store-boundary.py")).unwrap();
    std::fs::create_dir_all(f.root().join("tools")).unwrap();
    std::fs::write(f.root().join(path), &spec).unwrap();
    f.base = f.commit("unsupported base");
    std::fs::write(f.root().join(path), [spec.as_slice(), b"\n"].concat()).unwrap();
    f.loosen();
    let c = f.losses().unwrap();
    assert_eq!(c.unsupported, vec![path]);
    assert_eq!(c.losses.len(), 1);
    write(f.root(), DECLARATIONS, "{}");
    f.commit("bad declaration");
    assert!(f.losses().unwrap_err().contains("must be a list"));
}
#[test]
fn removing_an_advertised_adapter_fails_closed() {
    let f = Fixture::new();
    write(f.root(), GUARD, "pub fn no_adapter() {}\n");
    f.commit("remove adapter");
    assert!(f.losses().unwrap_err().contains("needs entries"));
}
#[test]
fn stdout_diagnostics_and_failed_runner_are_validated() {
    assert_eq!(
        parse_stdout(1, "scripts/writer:1: flag\n", "").unwrap(),
        BTreeSet::from([Finding {
            path: "scripts/writer".into(),
            line: 1,
            finding: "flag".into()
        }])
    );
    assert!(
        parse_stdout(1, "", "")
            .unwrap_err()
            .contains("guard exited 1 without findings")
    );
    assert!(
        parse_stdout(0, "noise\n", "")
            .unwrap_err()
            .contains("unstable guard diagnostic")
    );
    assert!(
        parse_stdout(2, "", "broken")
            .unwrap_err()
            .contains("guard exited 2")
    );
    assert!(parse_stdout(1, "../writer:1: flag", "").is_err());
}
#[test]
fn rust_exclusions_preserve_lifetimes_strings_and_shared_production_lines() {
    let source = "// #[cfg(test)]\nfn production() {}\n#[cfg(test)]\nmod tests {\n fn f(s: &'static str) { let s = r##\"}\"##; /* { /* } */ } */ }\n}\nfn still_production() {}\n";
    assert_eq!(
        rust_test_lines(source).unwrap(),
        BTreeSet::from([3, 4, 5, 6])
    );
    assert_eq!(
        rust_test_lines("#![cfg(test)]\nfn f() {}\n").unwrap(),
        BTreeSet::from([1, 2, 3])
    );
    assert_eq!(
        rust_test_lines("#[cfg(test)]\nmod tests {} fn production() {}\n").unwrap(),
        BTreeSet::from([1])
    );
    assert!(
        rust_test_lines("fn production() {} #[test] fn fixture() {}\n")
            .unwrap()
            .is_empty()
    );
    assert!(
        rust_test_lines("mod tests { #![cfg(test)] fn fixture() {} }\nfn production() {}\n")
            .unwrap()
            .is_empty()
    );
}
#[test]
fn historical_target_versions_keep_their_distinct_detector_semantics() {
    let source = "fn f(target: &Path) {\n let cache = root.join(\"target\");\n let cache = unrelated;\n std::fs::read(cache.join(\"new-thing\"));\n std::fs::read(target.join(\"debug\"));\n}\nfn unrelated(cache: &Path) {\n std::fs::read(cache.join(\"other\"));\n}\n";
    let names = |v| {
        target_reclaim::historical_entries(source, v)
            .into_iter()
            .map(|(_, n)| n)
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        names(HistoricalVersion::Global),
        BTreeSet::from(["new-thing".into(), "other".into()])
    );
    assert!(names(HistoricalVersion::Scoped).is_empty());
    assert_eq!(
        names(HistoricalVersion::Conservative),
        BTreeSet::from(["new-thing".into(), "debug".into()])
    );
}
#[test]
fn builtins_authenticate_all_five_known_historical_target_blobs() {
    let repo = repo_root();
    let source = "fn f(target: &Path) {\n std::fs::read(target.join(\"debug\"));\n}\n";
    let head = Snapshot::from([(
        "crates/x/src/f.rs".into(),
        Blob {
            oid: "unused".into(),
            bytes: source.as_bytes().to_vec(),
        },
    )]);
    for (commit, expected) in [
        ("4d7b15b43", false),
        ("6da4d5529", false),
        ("ce0c014e4", true),
        ("600fda2b2", false),
        ("652af6e1b2486872ff05b34708aacdacae0c282c", true),
    ] {
        let v = blob(&repo, commit, "scripts/check-target-reclaim.py")
            .unwrap()
            .unwrap();
        let result = BuiltinAdapters
            .collect(
                "scripts/check-target-reclaim.py",
                &v,
                commit,
                AdapterKind::Entries,
                &head,
                Instant::now() + ADAPTER_BUDGET,
            )
            .unwrap();
        assert_eq!(!result.is_empty(), expected, "{commit}");
    }
    let unknown = Blob {
        oid: "f".repeat(40),
        bytes: DETECT.as_bytes().into(),
    };
    assert!(
        BuiltinAdapters
            .collect(
                GUARD,
                &unknown,
                "unused",
                AdapterKind::Entries,
                &head,
                Instant::now() + ADAPTER_BUDGET
            )
            .unwrap_err()
            .contains("was not skipped")
    );
}

#[test]
fn frozen_historical_comparison_detects_the_scoped_alias_regression() {
    let temp = scratch_repository();
    let root = temp.path();
    fixture_git(root, &["init", "-q"]);
    let repo = repo_root();
    let path = "scripts/check-target-reclaim.py";
    let before = blob(&repo, "ce0c014e4", path).unwrap().unwrap();
    let after = blob(&repo, "6da4d5529", path).unwrap().unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(root.join(path), &before.bytes).unwrap();
    write(
        root,
        "crates/x/src/production.rs",
        "fn f(target: &Path) {\n std::fs::read(target.join(\"debug\"));\n}\n",
    );
    fixture_git(root, &["add", path, "crates/x/src/production.rs"]);
    fixture_git(root, &["commit", "-qm", "historical broad detector"]);
    let base = fixture_git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join(path), &after.bytes).unwrap();
    fixture_git(root, &["add", path]);
    fixture_git(root, &["commit", "-qm", "historical narrow detector"]);
    let c = compare(root, base.trim(), "HEAD", None).unwrap();
    assert_eq!(c.losses.len(), 1);
    let loss = &c.losses[0];
    assert_eq!(loss.guard, path);
    assert_eq!(loss.base_guard, before.oid);
    assert_eq!(loss.head_guard, Some(after.oid));
    assert_eq!(
        loss.finding,
        Finding {
            path: "crates/x/src/production.rs".into(),
            line: 2,
            finding: "debug".into()
        }
    );
    let (code, out, err) = crate::testutil::run(main, &[base.trim(), "HEAD"], &Ctx::new(root, &[]));
    assert_eq!(code, 1);
    assert!(err.is_empty());
    assert!(
        out.contains("lost production finding crates/x/src/production.rs:2: debug (UNDECLARED)")
    );
}
#[test]
fn stdout_opt_in_selects_both_versions_without_interpreting_source() {
    struct NativeStdout;
    impl HistoricalAdapters for NativeStdout {
        fn collect(
            &self,
            _guard: &str,
            version: &Blob,
            _commit: &str,
            kind: AdapterKind,
            _tree: &Snapshot,
            _deadline: Instant,
        ) -> Result<BTreeSet<Finding>, String> {
            assert_eq!(kind, AdapterKind::Stdout);
            let source = std::str::from_utf8(&version.bytes).unwrap();
            match source {
                "fn main() { println!(\"scripts/writer:1: flag\"); }\n" => {
                    parse_stdout(1, "scripts/writer:1: flag\n", "")
                }
                "// guard-regressions: stdout\nfn main() {}\n" => parse_stdout(0, "", ""),
                _ => Err("unstable guard diagnostic".into()),
            }
        }
    }
    let mut f = Fixture::new();
    write(
        f.root(),
        GUARD,
        "fn main() { println!(\"scripts/writer:1: flag\"); }\n",
    );
    f.base = f.commit("before opt in");
    write(
        f.root(),
        GUARD,
        "// guard-regressions: stdout\nfn main() {}\n",
    );
    f.commit("opt in while narrowing");
    let c = compare_with(f.root(), &f.base, "HEAD", None, &NativeStdout).unwrap();
    assert_eq!(c.losses[0].finding.finding, "flag");
    write(
        f.root(),
        GUARD,
        "// guard-regressions: stdout\nfn main() { panic!(); }\n",
    );
    f.commit("broken native adapter");
    assert!(
        compare_with(f.root(), &f.base, "HEAD", None, &NativeStdout)
            .unwrap_err()
            .contains("unstable guard diagnostic")
    );
}

#[test]
fn declaration_template_preserves_sorted_spaced_ascii_json() {
    let loss = Loss {
        guard: "scripts/check-example.py".into(),
        base_guard: "a".repeat(40),
        head_guard: None,
        finding: Finding {
            path: "scripts/writer".into(),
            line: 2,
            finding: "é😀".into(),
        },
        intended: false,
    };
    let template = declaration_template(&loss);
    assert_eq!(
        template,
        format!(
            "{{\"base_guard\": \"{}\", \"finding\": \"\\u00e9\\ud83d\\ude00\", \"guard\": \"scripts/check-example.py\", \"head_guard\": null, \"line\": 2, \"path\": \"scripts/writer\", \"reason\": \"REPLACE with reviewable reason\"}}",
            "a".repeat(40)
        )
    );
}

#[cfg(unix)]
#[test]
fn snapshot_keeps_non_utf8_and_backslash_paths_and_prints_legacy_escapes() {
    use crate::testutil::raw::{commit, tree};
    let temp = scratch_repository();
    let root = temp.path();
    fixture_git(root, &["init", "-q", "-b", "main"]);
    let raw: &[u8] = b"src/b\xff.rs";
    let head = commit(
        root,
        &tree(
            root,
            &[
                (raw, "fn f() {}\n"),
                (b"src/back\\slash.rs", "fn g() {}\n"),
                (b"notes.md", "not source\n"),
            ],
        ),
        &[],
    );
    let files = snapshot(root, &head).unwrap();
    let escaped = crate::os_escape::decode(raw).unwrap();
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["src/back\\slash.rs", escaped.as_str()]
    );
    let loss = Loss {
        guard: "scripts/check-x.py".into(),
        base_guard: "a".repeat(40),
        head_guard: None,
        finding: Finding {
            path: escaped,
            line: 3,
            finding: "flag".into(),
        },
        intended: false,
    };
    assert!(
        declaration_template(&loss).contains(r#""path": "src/b\udcff.rs""#),
        "{}",
        declaration_template(&loss)
    );
}

#[cfg(unix)]
mod executables {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn historical_native_guards_run_as_trusted_binaries_over_the_exported_head_tree() {
        let temp = scratch_repository();
        let root = temp.path();
        fixture_git(root, &["init", "-q", "-b", "main"]);
        write(root, GUARD, "// guard-regressions: stdout\nfn old() {}\n");
        write(root, "notes.txt", "kept\n");
        fixture_git(root, &["add", "--all"]);
        fixture_git(root, &["commit", "-qm", "base"]);
        let base = fixture_git(root, &["rev-parse", "HEAD"]).trim().to_string();
        write(
            root,
            GUARD,
            "// guard-regressions: stdout\nfn narrowed() {}\n",
        );
        fixture_git(root, &["add", "--all"]);
        fixture_git(root, &["commit", "-qm", "narrow"]);
        let head = fixture_git(root, &["rev-parse", "HEAD"]).trim().to_string();
        let bins = tempfile::tempdir().unwrap();
        // The base guard reports the head tree's notes.txt; the head guard is silent.
        let old = script(
            bins.path(),
            "old",
            r#"[ "$1" = target-reclaim ] && [ "$2" = --root ] || exit 9
[ "$(cat "$3/notes.txt")" = kept ] || exit 9
echo "notes.txt:1: kept finding"; exit 1"#,
        );
        let new = script(bins.path(), "new", "exit 0");
        let table = BinaryTable::parse(&[
            format!("{base}={}", old.display()),
            format!("{head}={}", new.display()),
        ])
        .unwrap();
        let environment = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let cancel = AtomicUsize::new(0);
        let adapters =
            ExecutableAdapters::new(&table, &BuiltinAdapters, root, &head, &environment, &cancel);
        let comparison = compare_with(root, &base, &head, None, &adapters).unwrap();
        assert_eq!(comparison.losses.len(), 1, "{comparison:?}");
        assert_eq!(comparison.losses[0].finding.path, "notes.txt");
        assert_eq!(comparison.losses[0].finding.finding, "kept finding");
        assert!(!comparison.losses[0].intended);

        // An unmaterialized commit fails closed instead of skipping the guard.
        let partial = BinaryTable::parse(&[format!("{head}={}", new.display())]).unwrap();
        let adapters = ExecutableAdapters::new(
            &partial,
            &BuiltinAdapters,
            root,
            &head,
            &environment,
            &cancel,
        );
        assert!(
            compare_with(root, &base, &head, None, &adapters)
                .unwrap_err()
                .contains("no trusted rsi-repo-guard is materialized")
        );
    }

    #[test]
    fn a_historical_guard_is_killed_at_its_deadline_and_bad_tables_are_refused() {
        let temp = scratch_repository();
        let root = temp.path();
        fixture_git(root, &["init", "-q", "-b", "main"]);
        write(root, "a.txt", "a\n");
        fixture_git(root, &["add", "--all"]);
        fixture_git(root, &["commit", "-qm", "base"]);
        let head = fixture_git(root, &["rev-parse", "HEAD"]).trim().to_string();
        let bins = tempfile::tempdir().unwrap();
        let stuck = script(bins.path(), "stuck", "echo started; sleep 60 & wait");
        let table = BinaryTable::parse(&[format!("{head}={}", stuck.display())]).unwrap();
        let environment = BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let cancel = AtomicUsize::new(0);
        let adapters =
            ExecutableAdapters::new(&table, &BuiltinAdapters, root, &head, &environment, &cancel);
        let started = Instant::now();
        let error = adapters
            .collect(
                GUARD,
                &Blob {
                    oid: "x".into(),
                    bytes: Vec::new(),
                },
                &head,
                AdapterKind::Stdout,
                &Snapshot::new(),
                Instant::now() + std::time::Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(error.contains("adapter deadline"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        for bad in ["nothash=/bin/true", &format!("{head}="), "plain"] {
            assert!(BinaryTable::parse(&[bad.to_string()]).is_err(), "{bad}");
        }
        assert!(BinaryTable::parse(&[format!("{head}=/a"), format!("{head}=/b")]).is_err());
    }
}
