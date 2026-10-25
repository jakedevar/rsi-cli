//! #3394: the compile probe's rules, attribution and tree building. The cargo
//! run itself is injected (`FakeChecker` reads the merged tree), so these tests
//! prove what the probe decides, not that rustc is installed.

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn paths(count: usize, extension: &str) -> Vec<String> {
    (0..count)
        .map(|n| format!("crates/x/src/f{n}.{extension}"))
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn many_rust_files_make_an_entry_wide_and_other_files_do_not_3394() {
    assert!(matches!(
        classify(&paths(WIDE_FILE_THRESHOLD, "rs"), false),
        Width::Wide(_)
    ));
    assert_eq!(
        classify(&paths(WIDE_FILE_THRESHOLD - 1, "rs"), false),
        Width::Narrow
    );
    assert_eq!(
        classify(&paths(WIDE_FILE_THRESHOLD * 4, "md"), false),
        Width::Narrow,
        "documentation cannot break a build"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_removed_public_item_of_a_shared_crate_makes_an_entry_wide_3394() {
    assert!(matches!(classify(&paths(2, "rs"), true), Width::Wide(_)));
    assert_eq!(classify(&paths(2, "rs"), false), Width::Narrow);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn only_a_removed_or_changed_public_line_is_a_public_surface_change_3394() {
    let removal = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-    pub fn old(a: u32) {}\n+    pub fn old(a: u64) {}\n";
    assert!(removes_public_item(removal));
    let restricted = "-pub(crate) struct Gone;\n";
    assert!(removes_public_item(restricted));
    let addition = "--- a/x.rs\n+++ b/x.rs\n@@ -0,0 +1 @@\n+pub fn brand_new() {}\n";
    assert!(
        !removes_public_item(addition),
        "an addition cannot break a caller"
    );
    assert!(!removes_public_item("-fn private() {}\n"));
    assert!(
        !removes_public_item("--- pub looks like a header\n"),
        "a diff header is not a removed line"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_red_check_needs_a_compiler_diagnostic_to_count_as_red_3394() {
    assert_eq!(interpret_check(Some(Some(0)), ""), CheckOutcome::Green);
    let red = interpret_check(
        Some(Some(101)),
        "crates/a/src/lib.rs:3:5: error[E0425]: cannot find value `names_in` in this scope\nerror: could not compile `a`\n",
    );
    let CheckOutcome::Red(diagnostics) = red else {
        panic!("{red:?}");
    };
    assert!(diagnostics.summary.contains("E0425"), "{diagnostics:?}");
    assert!(diagnostics.summary.contains("crates/a/src/lib.rs"));
    // cargo also exits 101 when it cannot reach the registry: no verdict.
    assert!(matches!(
        interpret_check(
            Some(Some(101)),
            "error: failed to download from `https://index.crates.io`\n"
        ),
        CheckOutcome::Unknown(_)
    ));
    assert!(matches!(
        interpret_check(None, "crates/a/src/lib.rs:3:5: error[E0425]: x"),
        CheckOutcome::Unknown(_)
    ));
    assert!(matches!(
        interpret_check(Some(None), ""),
        CheckOutcome::Unknown(_)
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn ahead(source: &str, paths: &[&str]) -> Ahead {
    Ahead {
        id: Uuid::new_v4(),
        source: source.to_owned(),
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn blame_names_the_entries_whose_files_the_diagnostics_mention_3394() {
    let (first, second) = (
        ahead("aaa", &["crates/a/src/x.rs"]),
        ahead("bbb", &["crates/b/src/y.rs"]),
    );
    let blamed = narrow_blame(
        "crates/b/src/y.rs:4:1: error[E0308]: mismatched",
        &[first.clone(), second.clone()],
    );
    assert_eq!(blamed, [(second.id, "bbb".to_owned())]);
    // A break no changed file explains is held against every entry ahead.
    let all = narrow_blame(
        "error: could not compile `z`",
        &[first.clone(), second.clone()],
    );
    assert_eq!(all.len(), 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_refusal_names_the_entries_ahead_and_the_first_error_3394() {
    let entry = Uuid::new_v4();
    let detail = refusal_detail(
        "tip0123",
        &Blame::Entries(vec![(entry, "0123456789abcdef0123".to_owned())]),
        "src/lib.rs:1:1: error[E0432]: unresolved import",
    );
    assert!(detail.starts_with("compile_conflict:"), "{detail}");
    assert!(detail.contains(&entry.to_string()), "{detail}");
    assert!(detail.contains("0123456789ab)"), "{detail}");
    assert!(detail.contains("E0432"), "{detail}");
    assert!(
        detail.contains("wait for those entries to land"),
        "{detail}"
    );
    let tip = refusal_detail("tip0123", &Blame::Tip, "e");
    assert!(tip.contains("the rolling tip tip0123 fails"), "{tip}");
}

// -------------------------------------------------------- tree building

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn sh(dir: &Path, script: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "{script}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn head(dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
/// A model of rustc: every `CALL name` token needs a `fn name` somewhere in the
/// tree. A missing definition is a red check whose diagnostics name the file.
pub(in crate::rolling_queue) struct FakeChecker {
    runs: std::sync::atomic::AtomicUsize,
    unknown: bool,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
impl FakeChecker {
    pub(in crate::rolling_queue) fn new() -> Self {
        Self {
            runs: std::sync::atomic::AtomicUsize::new(0),
            unknown: false,
        }
    }

    /// How many trees this checker compiled.
    pub(in crate::rolling_queue) fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() {
            rust_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
impl Checker for FakeChecker {
    fn check(&self, tree: &Path, _deadline: Instant) -> CheckOutcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.unknown {
            return CheckOutcome::Unknown("no cargo".to_owned());
        }
        let mut files = Vec::new();
        rust_files(tree, &mut files);
        let mut defined = Vec::new();
        let mut calls = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let relative = file.strip_prefix(tree).unwrap().display().to_string();
            for line in text.lines() {
                if let Some(name) = line.strip_prefix("fn ") {
                    defined.push(name.trim().to_owned());
                }
                if let Some(name) = line.strip_prefix("CALL ") {
                    calls.push((relative.clone(), name.trim().to_owned()));
                }
            }
        }
        let missing: Vec<String> = calls
            .iter()
            .filter(|(_, name)| !defined.contains(name))
            .map(|(file, name)| format!("{file}:1:1: error[E0425]: cannot find `{name}`"))
            .collect();
        match missing.is_empty() {
            true => CheckOutcome::Green,
            false => {
                let text = missing.join("\n");
                CheckOutcome::Red(Diagnostics {
                    summary: first_diagnostics(&text),
                    text,
                })
            }
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    tree_root: PathBuf,
    tip: String,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
/// A repository whose tip defines `old_api` in `lib.rs`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    sh(
        &repo,
        "git init -q -b rolling . && git config user.email t@t && git config user.name t \
         && printf 'fn old_api\\n' > lib.rs && printf 'filler\\n' > notes.txt \
         && git add -A && git commit -qm base",
    );
    let tip = head(&repo);
    Fixture {
        tree_root: dir.path().join("probe"),
        _dir: dir,
        repo,
        tip,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
impl Fixture {
    /// A commit cut from the tip that writes `files`.
    fn source(&self, branch: &str, files: &[(&str, &str)]) -> String {
        let mut script = format!("git checkout -q -B {branch} {}", self.tip);
        for (path, content) in files {
            script.push_str(&format!(
                " && printf '{content}\\n' > {path} && git add {path}"
            ));
        }
        script.push_str(&format!(" && git commit -qm {branch}"));
        sh(&self.repo, &script);
        head(&self.repo)
    }

    fn probe(&self, checker: &dyn Checker, ahead: &[Ahead], source: &str) -> Verdict {
        let deadline = Instant::now() + Duration::from_secs(60);
        let tree = ensure_tree(&self.tree_root, &self.repo, deadline).expect("probe tree");
        Prober { tree, checker }.probe(&self.tip, ahead, source, deadline)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_type_change_that_breaks_the_entry_queued_ahead_names_that_entry_3394() {
    let fixture = fixture();
    // A (ahead) calls the old API; green on the tip.
    let a = fixture.source("a", &[("caller.rs", "CALL old_api")]);
    // B (the wide refactor) renames it; green on the tip, red beside A.
    let b = fixture.source("b", &[("lib.rs", "fn new_api")]);
    let a_entry = ahead(&a, &["caller.rs"]);
    let checker = FakeChecker::new();
    match fixture.probe(&checker, std::slice::from_ref(&a_entry), &b) {
        Verdict::Breaks {
            blame: Blame::Entries(blamed),
            error,
        } => {
            assert_eq!(blamed, [(a_entry.id, a.clone())]);
            assert!(error.contains("caller.rs"), "{error}");
        }
        other => panic!("{other:?}"),
    }
    // Without A queued ahead the same refactor compiles.
    assert_eq!(fixture.probe(&checker, &[], &b), Verdict::Clear);
    assert_eq!(
        fixture.probe(&checker, &[a_entry], &a),
        Verdict::Clear,
        "an entry judged against itself ahead is just the entry"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_clear_entry_costs_one_check_3394() {
    let fixture = fixture();
    let a = fixture.source("a", &[("caller.rs", "CALL old_api")]);
    let b = fixture.source("b", &[("other.rs", "fn unrelated")]);
    let checker = FakeChecker::new();
    assert_eq!(
        fixture.probe(&checker, &[ahead(&a, &["caller.rs"])], &b),
        Verdict::Clear
    );
    assert_eq!(checker.runs(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_source_that_breaks_the_tip_alone_is_blamed_on_the_tip_3394() {
    let fixture = fixture();
    let a = fixture.source("a", &[("other.rs", "fn unrelated")]);
    let b = fixture.source("b", &[("caller.rs", "CALL ghost")]);
    let checker = FakeChecker::new();
    match fixture.probe(&checker, &[ahead(&a, &["other.rs"])], &b) {
        Verdict::Breaks {
            blame: Blame::Tip,
            error,
        } => assert!(error.contains("ghost"), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn an_entry_ahead_that_is_red_on_its_own_does_not_blame_the_source_3394() {
    let fixture = fixture();
    let a = fixture.source("a", &[("caller.rs", "CALL ghost")]);
    let b = fixture.source("b", &[("other.rs", "fn unrelated")]);
    let checker = FakeChecker::new();
    assert_eq!(
        fixture.probe(&checker, &[ahead(&a, &["caller.rs"])], &b),
        Verdict::NotBlamed
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn a_probe_that_cannot_decide_is_unknown_and_refuses_nothing_3394() {
    let fixture = fixture();
    let b = fixture.source("b", &[("other.rs", "fn unrelated")]);
    let checker = FakeChecker {
        runs: Default::default(),
        unknown: true,
    };
    assert!(matches!(
        fixture.probe(&checker, &[], &b),
        Verdict::Unknown(_)
    ));
    // A source that does not merge onto the entries ahead is the tip sweep's.
    let a = fixture.source("a", &[("lib.rs", "fn a_version")]);
    let conflicting = fixture.source("c", &[("lib.rs", "fn c_version")]);
    assert!(matches!(
        fixture.probe(&FakeChecker::new(), &[ahead(&a, &["lib.rs"])], &conflicting),
        Verdict::Unknown(_)
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn an_entry_ahead_that_does_not_merge_is_left_out_of_the_probe_3394() {
    let fixture = fixture();
    let first = fixture.source("first", &[("lib.rs", "fn first_version")]);
    let second = fixture.source("second", &[("lib.rs", "fn second_version")]);
    let b = fixture.source("b", &[("other.rs", "fn unrelated")]);
    // `second` conflicts with `first` ahead of it: skipped, `b` still judged.
    let entries = [ahead(&first, &["lib.rs"]), ahead(&second, &["lib.rs"])];
    assert_eq!(
        fixture.probe(&FakeChecker::new(), &entries, &b),
        Verdict::Clear
    );
}
