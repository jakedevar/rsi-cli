use super::*;
use crate::testutil::{repo_root, run, write};
use serde_json::json;
fn names(source: &str) -> BTreeSet<String> {
    entries(source).into_iter().map(|(_, n)| n).collect()
}
fn install(root: &Path) {
    for p in [POLICY, SHARED, RETAINED, NEXTEST] {
        let text = fs::read_to_string(repo_root().join(p)).unwrap();
        write(root, p, &text);
    }
}
#[test]
fn ported_current_tree_passes() {
    // #447: the scan took ~134 s in a dev build before the look-around
    // patterns left fancy-regex's VM (~50 s after, ~6 s in a release build).
    // The bound is generous for a loaded host and still catches a return to
    // the slow path; the landing guard's plan-only budget is 120 s.
    let started = std::time::Instant::now();
    assert_eq!(check(&repo_root()).unwrap(), Vec::<String>::new());
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(110),
        "check-target-reclaim over the current tree took {elapsed:?} (#447 bound 110 s in a dev build)"
    );
}
#[test]
fn python_embedded_functions_do_not_end_the_containing_scope() {
    let source = "def test_fixture():\n    data = '''def embedded(target):\n    target = root / \"target\"\n    '''\n    writer = repository / \"scripts/writer.sh\"\n    writer.write_text('mkdir -p target/new-thing\\n')\n";
    assert_eq!(entries(source), vec![(6, "new-thing".into())]);
    let masked = python_scope_code(source);
    assert_eq!(masked.len(), source.len());
    assert_eq!(masked.matches('\n').count(), source.matches('\n').count());
    assert!(!masked.contains("def embedded"));
    assert!(masked.contains("def test_fixture"));
    let retained =
        fs::read_to_string(repo_root().join("scripts/tests/test_check_target_reclaim.py")).unwrap();
    let masked = python_scope_code(&retained);
    assert!(masked.contains("def test_parameters_preserve_visible_target_bindings"));
    assert!(!masked.contains("fn target_identity_fixture"));
}
#[test]
fn multiline_literal_suffix_cannot_dedent_a_function() {
    // Read the retained source specification; never execute it. In particular
    // its interpolated constructor is unknown and grants no fixture exemption.
    let source =
        fs::read_to_string(repo_root().join("scripts/tests/test_check_target_reclaim.py")).unwrap();
    let scopes = Scopes::parse(&source);
    let at = source.find("copy.join(\"target/{'nested'}\")").unwrap();
    let (_, paths) = local_paths(&source, false);
    assert_eq!(paths.lookup("copy", at), Kind::Unknown);
    let previous = source
        .find("def test_own_target_admission_fixtures_are_isolated")
        .unwrap();
    let body = source[previous..].find("source =").unwrap() + previous;
    let suffix = source[body..].find(".replace(\"@\", \"\")").unwrap() + body;
    assert_eq!(scopes.at(body), scopes.at(suffix));
    assert_ne!(scopes.at(body), scopes.at(at));
}
#[test]
fn ported_policy_names_constants_and_fail_closed_grammar() {
    let policy = read_policy(&repo_root()).unwrap();
    assert_eq!(policy.directories, "debug release doc package tmp cargo-timings criterion dhat .rsi-tmp rsid-test-shards desktop-tauri e2e-artifacts nextest".split_whitespace().map(String::from).collect());
    assert_eq!(policy.files, ".rustc_info.json CACHEDIR.TAG .cargo-lock .future-incompat-report.json .rsid-test-shards.lock .rsi-job-admission.lock .rsi-release.lock .rsi-desktop.lock .rsi-build-env.json .rsi-target-pool.json python-discover.start python-discover.head python-discover.log".split_whitespace().map(String::from).collect());
    for n in ["rsid-test-fixtures", "issue211-idle-costs-123.jsonl"] {
        assert!(policy.allows(n));
    }
    for n in [
        "-fixtures",
        "issue211-idle-costs-.jsonl",
        "issue211-idle-costs-x.jsonl",
        "new-thing",
    ] {
        assert!(!policy.allows(n));
    }
    let t = tempfile::tempdir().unwrap();
    install(t.path());
    let source = fs::read_to_string(t.path().join(POLICY)).unwrap();
    write(
        t.path(),
        POLICY,
        &source.replacen("b\"debug\"\n", "b\"brand-new\" | b\"debug\"\n", 1),
    );
    let shared = fs::read_to_string(t.path().join(SHARED)).unwrap();
    write(
        t.path(),
        SHARED,
        &shared.replace("= \"e2e-artifacts\";", "= \"renamed-e2e\";"),
    );
    let policy = read_policy(t.path()).unwrap();
    assert!(policy.allows("brand-new"));
    assert!(policy.allows("renamed-e2e"));
    write(
        t.path(),
        POLICY,
        &source.replacen(
            "name == E2E_ARTIFACTS_DIR_NAME.as_bytes()",
            "name.starts_with(b\"new-\")",
            1,
        ),
    );
    assert!(
        read_policy(t.path())
            .unwrap_err()
            .contains("unsupported reclaim predicate")
    );
}
#[test]
fn ported_writer_spellings_and_top_level_joins() {
    for s in [
        "std::fs::create_dir_all(\"target/new-thing\");",
        "std::fs::write(root.join(\"target/new-thing/file\"), b\"data\");",
        "std::fs::create_dir_all(root.join(\"target\").join(\"new-thing\"));",
        "let cache = root.join(\"target\");\nstd::fs::create_dir_all(cache.join(\"new-thing\"));",
        "let cache: PathBuf = root.join(\"target\");\nstd::fs::create_dir_all(cache.join(\"new-thing\"));",
        "let cache = std::env::var_os(\"CARGO_TARGET_DIR\")\n    .map(PathBuf::from)\n    .unwrap_or_else(|| root.join(\"target\"));\nstd::fs::write(cache.join(\"new-thing/file\"), b\"x\");",
        "let base = std::env::var_os(\"CARGO_TARGET_DIR\")\n    .map(PathBuf::from)\n    .unwrap_or_else(|| root.join(\"target\"))\n    .join(\"new-thing\");\nstd::fs::create_dir_all(&base);",
        "mkdir -p target/new-thing",
        "mkdir -p \"$CARGO_TARGET_DIR/new-thing\"",
        "mkdir -p \"${CARGO_TARGET_DIR}/new-thing\"",
        "CACHE=\"$CARGO_TARGET_DIR\"\nmkdir -p \"$CACHE/new-thing\"",
        "CACHE=\"${CARGO_TARGET_DIR:-$ROOT/target}\"\nmkdir -p \"$CACHE/new-thing\"",
        "cargo build --target-dir target/new-thing",
        "cache = Path(os.environ[\"CARGO_TARGET_DIR\"])\n(cache / \"new-thing\").mkdir()",
        "cache = Path(os.environ.get(\"CARGO_TARGET_DIR\", Path(repo) / \"target\"))\nout = cache / \"new-thing\"\nout.mkdir()",
        "cache = Path(os.environ[\"CARGO_TARGET_DIR\"])\nos.makedirs(os.path.join(cache, \"new-thing\"))",
        "os.makedirs(os.path.join(os.environ[\"CARGO_TARGET_DIR\"], \"new-thing\"))",
        "out = Path(env[\"CARGO_TARGET_DIR\"]) / \"new-thing\"\nout.mkdir()",
    ] {
        assert_eq!(names(s), BTreeSet::from(["new-thing".into()]), "{s}");
    }
    assert_eq!(
        names(
            "let cache = root.join(\"target\");\nlet deps = cache.join(\"debug\");\nstd::fs::create_dir_all(deps.join(\"deps\"));\nstd::fs::write(root.join(\"target/.rsi-tmp/scoped-test/file\"), b\"x\");"
        ),
        BTreeSet::from(["debug".into(), ".rsi-tmp".into()])
    );
    assert_eq!(
        names(
            "cache = Path(os.environ.get(\"CARGO_TARGET_DIR\", str(Path.home() / \".cargo/shared-target\")))\n(cache / \"new-thing\").mkdir()"
        ),
        BTreeSet::from(["new-thing".into()])
    );
}
#[test]
fn ported_nextest_store_and_cli_retained_pairs() {
    assert_eq!(
        nextest_entries(&fs::read_to_string(repo_root().join(NEXTEST)).unwrap()).unwrap(),
        vec![(1, "nextest".into())]
    );
    assert!(
        nextest_entries("[store]\ndir = \"reports\"\n")
            .unwrap()
            .is_empty()
    );
    assert!(
        nextest_entries("[store]\ndir = 12\n")
            .unwrap_err()
            .contains("must be a string")
    );
    let t = tempfile::tempdir().unwrap();
    install(t.path());
    write(
        t.path(),
        NEXTEST,
        "[store]\ndir = \"target/new-thing/reports\"\n[profile.test-timings.junit]\npath = \"junit.xml\"\n",
    );
    assert!(
        check(t.path())
            .unwrap()
            .iter()
            .any(|f| f.starts_with(&format!("{NEXTEST}:1: target/new-thing")))
    );
    fs::remove_file(t.path().join(NEXTEST)).unwrap();
    write(
        t.path(),
        "scripts/writer.sh",
        "#!/bin/sh\nmkdir -p target/new-thing\n",
    );
    let (code, _, err) = run(
        main,
        &["--root", t.path().to_str().unwrap()],
        &Ctx::new(t.path(), &[]),
    );
    assert_eq!(code, 1);
    assert!(err.contains("scripts/writer.sh:2: target/new-thing"));
    assert!(err.contains(POLICY));
    assert!(err.contains(RETAINED));
    let mut r: serde_json::Value =
        serde_json::from_slice(&fs::read(t.path().join(RETAINED)).unwrap()).unwrap();
    r["scripts/writer.sh"] =
        json!({"entries":["new-thing"],"reason":"Fixture intentionally retains evidence."});
    write(t.path(), RETAINED, &serde_json::to_string(&r).unwrap());
    assert!(check(t.path()).unwrap().is_empty());
    write(t.path(), "scripts/writer.sh", "mkdir -p target/new-other\n");
    assert!(
        check(t.path())
            .unwrap()
            .iter()
            .any(|f| f.contains("target/new-other"))
    );
}
#[test]
fn ported_alias_scopes_binding_order_and_shadowing() {
    for s in [
        "fn production(root: &Path) {\n    let own = root.join(\"target\");\n    let copy = own;\n    std::fs::write(copy.join(\"new-thing\"), b\"x\");\n}\nfn unrelated(own: &Path, copy: &Path) {\n    std::fs::read(own.join(\"new-other\"));\n    std::fs::read(copy.join(\"new-other\"));\n}",
        "def production(root):\n    own = root / \"target\"\n    copy = own\n    (copy / \"new-thing\").mkdir()\n\ndef unrelated(own, copy):\n    (own / \"new-other\").read_bytes()\n    (copy / \"new-other\").read_bytes()\n",
        "production() {\n    own=\"$CARGO_TARGET_DIR\"\n    copy=\"$own\"\n    mkdir -p \"$copy/new-thing\"\n}\nunrelated() {\n    cat \"$own/new-other\" \"$copy/new-other\"\n}",
    ] {
        assert_eq!(entries(s), vec![(4, "new-thing".into())], "{s}");
    }
    let s = "fn production(root: &Path, cache: &Path) {\n    std::fs::read(cache.join(\"new-other\"));\n    let cache = root.join(\"target\");\n    {\n        std::fs::write(cache.join(\"new-thing\"), b\"x\");\n        let temporary = tempfile::tempdir().unwrap();\n        let cache = temporary.path();\n        std::fs::read(cache.join(\"new-other\"));\n    }\n    std::fs::write(cache.join(\"new-other\"), b\"x\");\n    {\n        let inner = root.join(\"target\");\n    }\n    std::fs::read(inner.join(\"new-other\"));\n}";
    assert_eq!(
        entries(s),
        vec![(5, "new-thing".into()), (10, "new-other".into())]
    );
}
#[test]
fn ported_normalization_parameters_and_unknown_rebindings() {
    for (var, child) in [("target_dir", "debug"), ("candidate", "scoped-baselines")] {
        let s = format!(
            "{var} = Path(os.environ.get(\"CARGO_TARGET_DIR\", Path(repo) / \"target\"))\nif not {var}.is_absolute():\n    {var} = Path(repo) / {var}\ncache = {var} / \"{child}\" / \"incremental\"\n"
        );
        assert_eq!(entries(&s), vec![(4, child.into())]);
    }
    for s in [
        "fn rsid_test_binary_sets(target: &Path) -> usize {\n    let entries = fs::read_dir(target.join(\"debug/deps\"));\n}",
        "def rsid_test_binary_sets(target):\n    return (target / \"debug/deps\").iterdir()\n",
    ] {
        assert_eq!(entries(s), vec![(2, "debug".into())]);
    }
    assert_eq!(
        entries(
            "cache = Path(os.environ[\"CARGO_TARGET_DIR\"])\ndef production(cache):\n    (cache / \"new-thing\").mkdir()\n"
        ),
        vec![(3, "new-thing".into())]
    );
    let s = "fn production(root: &Path) {\n    let cache = root.join(\"target\");\n    let cache = resolve(cache);\n    std::fs::write(cache.join(\"new-thing\"), b\"x\");\n    let copy = resolve(cache);\n    std::fs::write(copy.join(\"new-thing\"), b\"x\");\n    let cache = unrelated;\n    std::fs::write(cache.join(\"new-thing\"), b\"x\");\n    let temporary = tempfile::tempdir().unwrap();\n    let cache = temporary.path();\n    std::fs::write(cache.join(\"new-other\"), b\"x\");\n}";
    assert_eq!(
        entries(s),
        vec![
            (4, "new-thing".into()),
            (6, "new-thing".into()),
            (8, "new-thing".into())
        ]
    );
}
#[test]
fn ported_fixture_provenance_escapes_and_parameter_shadowing() {
    for constructor in [
        "tempfile::tempdir().unwrap()",
        "TempDir::new().unwrap()",
        "crate::test_support::disk_backed_tempdir(\"target guard\")",
    ] {
        let s = format!(
            "fn fixture() {{\n    let temporary = {constructor};\n    let root = temporary.path();\n    let copy = root;\n    std::fs::write(copy.join(\"target/nested\"), b\"x\");\n    std::fs::write({constructor}.path().join(\"target/target\"), b\"x\");\n}}\nfn production(root: &Path) {{\n    let target = root.join(\"target\");\n    std::fs::write(target.join(\"new-thing\"), b\"x\");\n}}"
        );
        assert_eq!(entries(&s), vec![(10, "new-thing".into())]);
    }
    for s in [
        "let fixture = repository;\nstd::fs::write(fixture.join(\"target/new-thing\"), b\"x\");",
        "let fixture = tempfile::tempdir().unwrap();\nlet cache = std::env::var_os(\"CARGO_TARGET_DIR\")\n    .map(PathBuf::from)\n    .unwrap_or_else(|| fixture.path().join(\"target\"));\nstd::fs::write(cache.join(\"new-thing\"), b\"x\");",
        "let fixture = tempfile::tempdir().unwrap();\nstd::fs::write(fixture.path().join(\"../target/new-thing\"), b\"x\");",
        "let fixture = tempfile::tempdir().unwrap();\nstd::fs::write(fixture.path().join(\"/repo/target/new-thing\"), b\"x\");",
        "fn fixture() {\n    let temporary = tempfile::tempdir().unwrap();\n    let root = temporary.path();\n    {\n        let root = repository;\n        std::fs::write(root.join(\"target/new-thing\"), b\"x\");\n    }\n    std::fs::write(root.join(\"target/new-other\"), b\"x\");\n}",
    ] {
        assert_eq!(names(s), BTreeSet::from(["new-thing".into()]), "{s}");
    }
    assert_eq!(
        entries(
            "temporary = tempfile.TemporaryDirectory()\nroot = Path(temporary.name)\n(root / \"target/new-other\").mkdir()\ndef production(root):\n    (root / \"target/new-thing\").mkdir()\n"
        ),
        vec![(5, "new-thing".into())]
    );
    assert_eq!(
        entries(
            "def production(root):\n    root = Path(os.environ[\"CARGO_TARGET_DIR\"])\n    (root / \"new-thing\").mkdir()\n"
        ),
        vec![(3, "new-thing".into())]
    );
    assert_eq!(
        entries(
            "fn production(root: &Path) {\n    let cache = root.join(\"target\");\n    println!(\"}\"); // }\n    std::fs::write(cache.join(\"new-thing\"), b\"x\");\n}\nfn unrelated(cache: &Path) {\n    std::fs::read(cache.join(\"new-other\"));\n}"
        ),
        vec![(4, "new-thing".into())]
    );
}
#[test]
fn disk_backed_stamped_helper_requires_local_proof_and_fixture_argument() {
    let helper = "#[cfg(test)]\nmod tests {\nfn stamped_repository(temp: &tempfile::TempDir) -> PathBuf {\n    let repository = temp.path().join(\"repository\");\n    std::fs::create_dir_all(&repository).unwrap();\n    repository\n}\n";
    let case = "fn fixture() {\nlet temp = crate::test_support::disk_backed_tempdir(\"settlement\");\nlet repository = stamped_repository(&temp);\nstd::fs::write(repository.join(\"target/authored.txt\"), b\"x\");\n}\n}\n";
    assert!(entries(&format!("{helper}{case}")).is_empty());
    for bad in [
        helper.replace("temp.path()", "production.path()"),
        helper.replace("join(\"repository\")", "join(\"../repository\")"),
        helper.replace("    repository\n", "    return production;\n"),
        helper.replace("#[cfg(test)]", ""),
        helper.replace(
            "std::fs::create_dir_all(&repository).unwrap();",
            "let repository = production;",
        ),
    ] {
        assert_eq!(
            names(&format!("{bad}{case}")),
            BTreeSet::from(["authored.txt".into()])
        );
    }
    assert_eq!(
        names(&format!(
            "{helper}{}",
            case.replace(
                "crate::test_support::disk_backed_tempdir(\"settlement\")",
                "production_root"
            )
        )),
        BTreeSet::from(["authored.txt".into()])
    );
    assert_eq!(
        names(&format!(
            "{helper}{case}\nstd::fs::write(\"target/authored.txt\", b\"x\");"
        )),
        BTreeSet::from(["authored.txt".into()])
    );
}
#[test]
fn ported_own_target_admission_fixtures_are_isolated() {
    let source = "fn eligible_target(sandbox: &Path) {\n    let root = sandbox.canonicalize().unwrap();\n    let own = root.join(\"target\");\n}\nfn target_identity_fixture() {\n    let fixture = tempfile::tempdir().unwrap();\n    let root = fixture.path().canonicalize().unwrap();\n    let own = root.join(\"own\");\n    let other = root.join(\"other\");\n    std::fs::create_dir_all(own.join(\"nested\")).unwrap();\n    std::fs::create_dir_all(other.join(\"target\")).unwrap();\n    let target = own.join(\"target\");\n    std::fs::create_dir_all(target.join(\"target\")).unwrap();\n    std::fs::create_dir_all(root.join(\"target/nested\")).unwrap();\n    std::fs::create_dir_all(root.join(\"target/target\")).unwrap();\n}\nfn own_target_bytes_fixture() {\n    let fixture = tempfile::tempdir().unwrap();\n    let own = fixture.path().join(\"own/target\");\n    let other = fixture.path().join(\"other/target\");\n    std::fs::write(own.join(\"artifact\"), b\"x\");\n    std::os::unix::fs::symlink(&other, own.join(\"sibling\"));\n}";
    assert!(entries(source).is_empty());
}
fn lookaround_cases() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("mkdir -p target/new-thing", vec!["new-thing"]),
        ("mkdir -p target/new-thing;", vec!["new-thing"]),
        ("mkdir -p target/new-thing\u{a0}", vec!["new-thing"]),
        ("mkdir -p target/new-thing/child", vec!["new-thing"]),
        ("x=target/new-thing)", vec!["new-thing"]),
        // The look-behind: a word character or `-` before `target/`.
        ("mkdir -p x-target/new-thing", vec![]),
        ("mkdir -p xtarget/new-thing", vec![]),
        ("mkdir -p \u{e9}target/new-thing", vec![]),
        // The look-ahead: the component must end at `/`, a space, a quote or
        // closer, or the end of the text.
        ("mkdir -p target/new-thing=1", vec![]),
        ("mkdir -p target/new-thing\u{e9}", vec![]),
        ("mkdir -p \"$CARGO_TARGET_DIR/new-thing=\"", vec![]),
        (
            "let cache = root.join(\"target\");\nstd::fs::create_dir_all(cache.join(\"new-thing\u{e9}\"));",
            vec![],
        ),
        (
            "let cache = root.join(\"target\");\nstd::fs::create_dir_all(cache.join(\"new-thing\"));",
            vec!["new-thing"],
        ),
        (
            "os.makedirs(os.path.join(os.environ[\"CARGO_TARGET_DIR\"], \"new-thing\"))",
            vec!["new-thing"],
        ),
        (
            "os.makedirs(os.path.join(os.environ[\"CARGO_TARGET_DIR\"], \"new-thing\u{e9}\"))",
            vec![],
        ),
        // `=(?!=)`: a comparison is no assignment.
        (
            "let cache == root.join(\"target\");\nstd::fs::create_dir_all(cache.join(\"new-thing\"));",
            vec![],
        ),
        (
            "let cache = = root.join(\"target\");\nstd::fs::create_dir_all(cache.join(\"new-thing\"));",
            vec!["new-thing"],
        ),
    ]
}
#[test]
fn lookaround_free_patterns_keep_the_original_findings() {
    for (source, expected) in lookaround_cases() {
        let expected: BTreeSet<String> = expected.into_iter().map(String::from).collect();
        assert_eq!(names(source), expected, "{source:?}");
    }
}
