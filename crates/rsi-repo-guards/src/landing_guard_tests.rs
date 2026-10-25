use super::*;
use crate::scratch_git::Policy;
use crate::testutil::{repo_root, write};

struct Fixture {
    temp: tempfile::TempDir,
    policy: Policy,
    base: String,
    cancel: AtomicUsize,
}
impl Fixture {
    fn new() -> Self {
        let checkout = repo_root();
        let temp = Policy::from_process(&checkout)
            .unwrap()
            .tempdir("rsi-landing-guard-")
            .unwrap();
        let mut env: Environment = ["PATH", "SYSTEMROOT", "SystemRoot", "PATHEXT"]
            .iter()
            .filter_map(|key| std::env::var_os(key).map(|v| ((*key).into(), v)))
            .collect();
        env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        env.insert(
            "GIT_CONFIG_GLOBAL".into(),
            if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
        );
        let policy = Policy::new(&checkout, &checkout, env, &[]).unwrap();
        let mut f = Self {
            temp,
            policy,
            base: String::new(),
            cancel: AtomicUsize::new(0),
        };
        f.git(&["init", "-q", "-b", "trunk"]);
        f.git(&["config", "user.name", "landing-fixture"]);
        f.git(&["config", "user.email", "landing-fixture@example.invalid"]);
        write(
            f.root(),
            "crates/fixture/Cargo.toml",
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        );
        f.set("old default\nobsolete line\nkeep\n");
        f.base = f.commit("base");
        f
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn git(&self, args: &[&str]) -> String {
        let mut pinned = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ];
        pinned.extend_from_slice(args);
        let done = self
            .policy
            .command(self.root(), &pinned)
            .unwrap()
            .output()
            .unwrap();
        assert!(
            done.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&done.stderr)
        );
        String::from_utf8(done.stdout).unwrap().trim().into()
    }
    fn set(&self, text: &str) {
        write(self.root(), "crates/fixture/src/lib.rs", text);
    }
    fn commit(&self, message: &str) -> String {
        self.git(&["add", "crates/fixture"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn repo(&self) -> Repository<'_> {
        Repository::new(
            self.root(),
            &std::env::vars_os().collect(),
            Instant::now() + Duration::from_secs(60),
            &self.cancel,
        )
        .unwrap()
    }
    fn findings(&self, base: &str, source: &str, candidate: &str) -> Vec<String> {
        self.repo().lost_hunks(base, source, candidate).unwrap()
    }
    fn changed(&self) -> String {
        self.set("new default\nobsolete line\nkeep\n");
        self.commit("accepted default")
    }
    fn stale_merge(&mut self) -> (String, String, String) {
        write(
            self.root(),
            "crates/fixture/src/other.rs",
            "fn first() {}\nfn last() {}\n",
        );
        self.base = self.commit("base with other file");
        self.git(&["switch", "-q", "-c", "source"]);
        self.changed();
        self.git(&["switch", "-q", "trunk"]);
        write(
            self.root(),
            "crates/fixture/src/other.rs",
            "fn first() {}\nfn test_stale_lock_is_preserved() {}\nfn last() {}\n",
        );
        self.commit("rolling adds test");
        self.git(&["switch", "-q", "source"]);
        self.git(&["merge", "-q", "--no-ff", "-m", "older rolling", "trunk"]);
        let source = self.git(&["rev-parse", "HEAD"]);
        self.git(&["switch", "-q", "trunk"]);
        write(
            self.root(),
            "crates/fixture/src/other.rs",
            "fn first() {}\nfn last() {}\nfn test_stale_lock_is_preserved_and_cargo_runs() {}\n",
        );
        let target = self.commit("rolling renames test");
        self.git(&["merge", "-q", "--no-ff", "-m", "land source", &source]);
        (source, target, self.git(&["rev-parse", "HEAD"]))
    }
    fn criss_cross(&mut self) -> (String, String, String, String, String) {
        write(
            self.root(),
            "crates/fixture/src/other.rs",
            "use a::{\n    a1, a2, a3,\n    a4, a5,\n};\n",
        );
        write(
            self.root(),
            "crates/fixture/src/extra.rs",
            "use b::{\n    b1, b2, b3,\n    b4, b5,\n};\n",
        );
        self.base = self.commit("base with two files");
        self.git(&["switch", "-q", "-c", "source"]);
        write(
            self.root(),
            "crates/fixture/src/extra.rs",
            "use b::{\n    b1, b2, b3, b0,\n    b4, b5,\n};\n",
        );
        let s1 = self.commit("source shared line");
        self.git(&["switch", "-q", "trunk"]);
        write(
            self.root(),
            "crates/fixture/src/other.rs",
            "use a::{\n    a1, a2, a3, a0,\n    a4, a5,\n};\n",
        );
        let m1 = self.commit("rolling line");
        self.git(&["merge", "-q", "--no-ff", "-m", "rolling takes source", &s1]);
        self.git(&["switch", "-q", "source"]);
        self.git(&["merge", "-q", "--no-ff", "-m", "source takes rolling", &m1]);
        let source = self.changed();
        self.git(&["switch", "-q", "trunk"]);
        write(
            self.root(),
            "crates/fixture/src/other.rs",
            "use a::{\n    a1, a2, a3,\n    a4, a5, a0,\n};\n",
        );
        write(
            self.root(),
            "crates/fixture/src/extra.rs",
            "use b::{\n    b1, b2, b3,\n    b4, b5, b0,\n};\n",
        );
        let target = self.commit("rolling rewraps both");
        self.git(&["merge", "-q", "--no-ff", "-m", "land source", &source]);
        (s1, m1, source, target, self.git(&["rev-parse", "HEAD"]))
    }
}
#[derive(Default)]
struct Checks {
    policy: usize,
    tests: usize,
    fail_policy: bool,
    fail_test: bool,
}
impl Backend for Checks {
    fn check_candidate(&mut self, _: &Repository<'_>, _: &str) -> io::Result<()> {
        self.policy += 1;
        if self.fail_policy {
            Err(error("candidate policy red"))
        } else {
            Ok(())
        }
    }
    fn run_tests(&mut self, _: &str, commands: &[Vec<String>]) -> io::Result<()> {
        self.tests += 1;
        assert_eq!(commands.len(), 1);
        if self.fail_test {
            Err(error("tests red"))
        } else {
            Ok(())
        }
    }
}
#[test]
fn empty_accepted_delta_reports_already_on_rolling() {
    let f = Fixture::new();
    let mut checks = Checks::default();
    let pair = Accepted {
        base: &f.base,
        source: &f.base,
        target: &f.base,
        candidate: &f.base,
    };
    let (code, report) = evaluate(&f.repo(), &pair, &[], true, &mut checks).unwrap();
    assert_eq!(code, 0);
    assert_eq!(report["outcome"], "already_on_rolling");
    assert_eq!(report["candidate"], f.base);
    assert_eq!(checks.policy, 1);
    assert_eq!(checks.tests, 0);
}
#[test]
fn reports_exact_accepted_line_reverted_by_later_resolution() {
    let f = Fixture::new();
    let source = f.changed();
    f.set("old default\nobsolete line\nkeep\n");
    let candidate = f.commit("bad resolution");
    let findings = f.findings(&f.base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("restored base line 'old default'"));
    assert!(findings[0].contains("replaced by accepted 'new default'"));
    assert_eq!(
        f.repo().affected_crates(&source, &candidate).unwrap(),
        vec!["fixture"]
    );
}
#[test]
fn accepts_unrelated_followup_and_selects_affected_crate() {
    let f = Fixture::new();
    let source = f.changed();
    write(
        f.root(),
        "crates/fixture/src/other.rs",
        "pub fn other() {}\n",
    );
    let candidate = f.commit("followup");
    assert_eq!(
        f.findings(&f.base, &source, &candidate),
        Vec::<String>::new()
    );
    assert_eq!(
        f.repo().affected_crates(&source, &candidate).unwrap(),
        vec!["fixture"]
    );
}
#[test]
fn detects_pure_deletion_restored() {
    let f = Fixture::new();
    f.set("old default\nkeep\n");
    let source = f.commit("deletion");
    f.set("old default\nobsolete line\nkeep\n");
    let candidate = f.commit("restoration");
    let findings = f.findings(&f.base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("restored base line 'obsolete line'"));
}
#[test]
fn accepts_later_evolution_of_accepted_line() {
    let f = Fixture::new();
    let source = f.changed();
    f.set("newer default\nobsolete line\nkeep\n");
    let candidate = f.commit("evolution");
    assert!(f.findings(&f.base, &source, &candidate).is_empty());
}
#[test]
fn accepts_later_rename_and_edit() {
    let f = Fixture::new();
    let source = f.changed();
    f.git(&[
        "mv",
        "crates/fixture/src/lib.rs",
        "crates/fixture/src/renamed.rs",
    ]);
    write(
        f.root(),
        "crates/fixture/src/renamed.rs",
        "newer default\nobsolete line\nkeep\n",
    );
    let candidate = f.commit("rename evolution");
    assert_eq!(
        f.repo()
            .renamed_paths(&source, &candidate)
            .unwrap()
            .get(Path::new("crates/fixture/src/lib.rs")),
        Some(&PathBuf::from("crates/fixture/src/renamed.rs"))
    );
    assert!(f.findings(&f.base, &source, &candidate).is_empty());
}
#[test]
fn reports_rollback_after_rename_at_new_path() {
    let f = Fixture::new();
    let source = f.changed();
    f.git(&[
        "mv",
        "crates/fixture/src/lib.rs",
        "crates/fixture/src/renamed.rs",
    ]);
    write(
        f.root(),
        "crates/fixture/src/renamed.rs",
        "old default\nobsolete line\nkeep\n",
    );
    let candidate = f.commit("renamed rollback");
    let findings = f.findings(&f.base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("renamed.rs:1: restored base line 'old default'"));
}
#[test]
fn reports_missing_addition_with_unrelated_later_edit() {
    let f = Fixture::new();
    f.set("old default\naccepted addition\nobsolete line\nkeep\n");
    let source = f.commit("addition");
    f.set("old default\nobsolete line\nkeep\nunrelated edit\n");
    let candidate = f.commit("lost insertion");
    let findings = f.findings(&f.base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("accepted insertion 'accepted addition'"));
}
#[test]
fn identical_attribute_occurrences_survive_boundary_remapping() {
    let f = Fixture::new();
    f.set("#[allow(clippy::unwrap_used)]\nfn first() {}\n#[allow(clippy::unwrap_used)]\nfn second() {}\nold default\nobsolete line\nkeep\n");
    let source = f.commit("duplicate attributes");
    f.set("fn prelude() {}\n#[allow(clippy::unwrap_used)]\nfn first() {}\n#[allow(clippy::unwrap_used)]\nfn second() {}\nold default\nobsolete line\nkeep\n");
    let candidate = f.commit("shift boundary");
    assert!(f.findings(&f.base, &source, &candidate).is_empty());
}
#[test]
fn new_duplicate_elsewhere_does_not_hide_lost_insertion() {
    let f = Fixture::new();
    f.set("fn a() {}\nfn b() {}\nfn c() {}\n");
    let base = f.commit("functions");
    f.set("fn a() {}\n#[tokio::test]\nfn b() {}\nfn c() {}\n");
    let source = f.commit("attribute on b");
    f.set("fn a() {}\nfn b() {}\nfn c() {}\n#[tokio::test]\nfn d() {}\n");
    let candidate = f.commit("different attribute");
    let findings = f.findings(&base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("accepted insertion '#[tokio::test]'"));
}
#[test]
fn lost_hunk_still_runs_affected_crate_tests() {
    let f = Fixture::new();
    let source = f.changed();
    f.set("old default\nobsolete line\nkeep\n");
    let candidate = f.commit("rollback");
    let mut checks = Checks::default();
    let pair = Accepted {
        base: &f.base,
        source: &source,
        target: &source,
        candidate: &candidate,
    };
    let (code, report) = evaluate(&f.repo(), &pair, &[], false, &mut checks).unwrap();
    assert_eq!(code, 1);
    assert_eq!(checks.policy, 1);
    assert_eq!(checks.tests, 1);
    assert_eq!(report["tests_executed"], true);
    assert!(report["lost_hunks"].as_array().unwrap().len() == 1);
    checks.fail_test = true;
    assert!(evaluate(&f.repo(), &pair, &[], false, &mut checks).is_err());
}
#[test]
fn accepts_evolved_insertion_at_same_boundary() {
    let f = Fixture::new();
    f.set("old default\naccepted addition\nobsolete line\nkeep\n");
    let source = f.commit("addition");
    f.set("old default\nevolved addition\nobsolete line\nkeep\n");
    let candidate = f.commit("evolved addition");
    assert!(f.findings(&f.base, &source, &candidate).is_empty());
}
#[test]
fn binary_evolution_passes_but_exact_rollback_fails() {
    let f = Fixture::new();
    write(f.root(), "crates/fixture/src/asset.bin", "\0base");
    let base = f.commit("binary base");
    write(f.root(), "crates/fixture/src/asset.bin", "\0accepted");
    let source = f.commit("binary accepted");
    write(f.root(), "crates/fixture/src/asset.bin", "\0evolved");
    let evolved = f.commit("binary evolved");
    assert!(f.findings(&base, &source, &evolved).is_empty());
    write(f.root(), "crates/fixture/src/asset.bin", "\0base");
    let candidate = f.commit("binary rollback");
    let findings = f.findings(&base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("accepted binary edit reverted to base blob sha256:"));
}
#[test]
fn filter_is_explicit_and_may_target_readers() {
    let packages = vec!["fixture".into()];
    let commands = test_commands(&packages, &["fixture=accepted_default".into()]).unwrap();
    assert_eq!(
        commands[0],
        vec![
            "cargo",
            "test",
            "-p",
            "fixture",
            "--lib",
            "accepted_default"
        ]
    );
    assert!(test_commands(&packages, &["fixture=".into()]).is_err());
    let commands = test_commands(
        &packages,
        &["other=bin:tool".into(), "other=test:corpus".into()],
    )
    .unwrap();
    assert_eq!(
        commands,
        vec![
            vec!["cargo", "test", "-p", "fixture", "--lib"],
            vec!["cargo", "test", "-p", "other", "--bin", "tool"],
            vec!["cargo", "test", "-p", "other", "--test", "corpus"]
        ]
    );
}
#[test]
fn merged_in_rolling_content_later_renamed_is_not_a_reversion() {
    let mut f = Fixture::new();
    let (source, target, candidate) = f.stale_merge();
    let stale = f.findings(&f.base, &source, &candidate);
    assert_eq!(stale.len(), 1);
    assert!(stale[0].contains("accepted insertion 'fn test_stale_lock_is_preserved() {}'"));
    let base = f.repo().own_base(&f.base, &source, &target).unwrap();
    assert!(f.findings(&base, &source, &candidate).is_empty());
}
#[test]
fn real_reversion_is_still_refused_after_stale_merge() {
    let mut f = Fixture::new();
    let (source, target, _) = f.stale_merge();
    f.set("old default\nobsolete line\nkeep\n");
    let candidate = f.commit("own rollback");
    let base = f.repo().own_base(&f.base, &source, &target).unwrap();
    let findings = f.findings(&base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("restored base line 'old default'"));
}
#[test]
fn criss_cross_source_is_measured_against_its_virtual_merge_base() {
    let mut f = Fixture::new();
    let (s1, m1, source, target, candidate) = f.criss_cross();
    let mut bases = words(
        &f.repo()
            .git(&["merge-base", "--all", &source, &target], true)
            .unwrap(),
    )
    .unwrap();
    bases.sort();
    let mut expected = vec![s1.clone(), m1.clone()];
    expected.sort();
    assert_eq!(bases, expected);
    for base in [&s1, &m1] {
        assert!(!f.findings(base, &source, &candidate).is_empty());
    }
    let base = f.repo().own_base(&f.base, &source, &target).unwrap();
    assert_ne!(base, f.base);
    assert_ne!(base, s1);
    assert_ne!(base, m1);
    assert!(f.findings(&base, &source, &candidate).is_empty());
}
#[test]
fn real_reversion_is_still_refused_with_several_merge_bases() {
    let mut f = Fixture::new();
    let (_, _, source, target, _) = f.criss_cross();
    f.set("old default\nobsolete line\nkeep\n");
    let candidate = f.commit("own rollback");
    let base = f.repo().own_base(&f.base, &source, &target).unwrap();
    let findings = f.findings(&base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("restored base line 'old default'"));
}
#[test]
fn own_base_never_widens_the_accepted_delta() {
    let f = Fixture::new();
    let source = f.changed();
    let repo = f.repo();
    assert_eq!(repo.own_base(&f.base, &source, &source).unwrap(), f.base);
    assert_eq!(repo.own_base(&f.base, &source, &f.base).unwrap(), f.base);
    assert_eq!(repo.own_base(&source, &source, &f.base).unwrap(), source);
}
#[test]
fn deleted_file_and_missing_path_semantics_match_legacy() {
    let f = Fixture::new();
    fs_remove(f.root().join("crates/fixture/src/lib.rs"));
    let source = f.commit("file deletion");
    f.set("old default\nobsolete line\nkeep\n");
    let candidate = f.commit("file restoration");
    let findings = f.findings(&f.base, &source, &candidate);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("accepted file deletion restored base blob sha256:"));
    let f = Fixture::new();
    let source = f.changed();
    fs_remove(f.root().join("crates/fixture/src/lib.rs"));
    let candidate = f.commit("removed path");
    assert!(f.findings(&f.base, &source, &candidate).is_empty());
}
fn fs_remove(path: PathBuf) {
    std::fs::remove_file(path).unwrap();
}
#[test]
fn candidate_policy_is_required_in_plan_only_and_pair_binding_is_strict() {
    let f = Fixture::new();
    let source = f.changed();
    let pair = Accepted {
        base: &f.base,
        source: &source,
        target: &f.base,
        candidate: &source,
    };
    let mut checks = Checks {
        fail_policy: true,
        ..Checks::default()
    };
    assert!(evaluate(&f.repo(), &pair, &[], true, &mut checks).is_err());
    assert_eq!(checks.policy, 1);
    assert_eq!(checks.tests, 0);
    let pair = Accepted {
        base: &source,
        source: &f.base,
        target: &f.base,
        candidate: &source,
    };
    assert!(
        pair.validate(&f.repo())
            .unwrap_err()
            .to_string()
            .contains("accepted base is not an ancestor")
    );
    let bad = source.to_ascii_uppercase();
    assert!(f.repo().commit(&bad).is_err());
}
#[test]
fn line_bytes_and_matching_ties_are_preserved_without_autojunk() {
    assert_eq!(
        lines(b"a\r\nb\rc\nlast"),
        vec![&b"a\r\n"[..], &b"b\r"[..], &b"c\n"[..], &b"last"[..]]
    );
    assert_eq!(line_text(b"bad\xff\n"), "'bad\\\\xff'");
    let a = lines(b"a\nb\na\n");
    let b = lines(b"b\na\nb\n");
    assert_eq!(
        matching_blocks(&a, &b, &|| Ok(())).unwrap(),
        vec![Match { a: 0, b: 1, len: 2 }, Match { a: 3, b: 3, len: 0 }]
    );
    let repeated = vec![&b"repeat\n"[..]; 250];
    assert_eq!(
        matching_blocks(&repeated, &repeated, &|| Ok(())).unwrap()[0].len,
        250
    );
}
#[test]
fn metadata_and_matcher_cancellation_fail_closed() {
    let f = Fixture::new();
    f.cancel.store(15, Ordering::Relaxed);
    assert_eq!(
        f.repo().paths_changed(&f.base, &f.base).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    let uncancelled = AtomicUsize::new(0);
    let expired = Repository::new(
        f.root(),
        &Environment::new(),
        Instant::now() - Duration::from_millis(1),
        &uncancelled,
    );
    assert_eq!(
        expired
            .unwrap()
            .paths_changed(&f.base, &f.base)
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let check = || Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    assert_eq!(
        matching_blocks(&lines(b"a"), &lines(b"b"), &check)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn native_git_paths_preserve_newlines_and_source_data_extensions() {
    let f = Fixture::new();
    let path = Path::new("crates/fixture/src/odd\né.rs");
    write(f.root(), path.to_str().unwrap(), "before\n");
    let base = f.commit("path base");
    write(f.root(), path.to_str().unwrap(), "accepted\n");
    let source = f.commit("path accepted");
    assert_eq!(
        f.repo().paths_changed(&base, &source).unwrap(),
        vec![path.to_owned()]
    );
    assert_eq!(
        f.repo().blob(&source, path).unwrap(),
        Some(b"accepted\n".to_vec())
    );
    write(f.root(), path.to_str().unwrap(), "before\n");
    let candidate = f.commit("path reverted");
    assert_eq!(f.findings(&base, &source, &candidate).len(), 1);
    let env = Environment::new();
    assert!(
        process::stream_git_bytes(
            f.root(),
            &["python3".into()],
            &env,
            Instant::now() + Duration::from_secs(1),
            &f.cancel,
            |_, _| {}
        )
        .is_err()
    );
    assert!(
        process::stream_git_bytes(
            f.root(),
            &["external-command".into()],
            &env,
            Instant::now() + Duration::from_secs(1),
            &f.cancel,
            |_, _| {}
        )
        .is_err()
    );
    // Read the retained legacy module as committed data, never execute it. This
    // distinguishes source filenames from Python commands in the shared pump.
    let checkout = repo_root();
    let repo = Repository::new(
        &checkout,
        &std::env::vars_os().collect(),
        Instant::now() + Duration::from_secs(10),
        &f.cancel,
    )
    .unwrap();
    assert!(
        repo.blob("HEAD", Path::new("scripts/scratch_git.py"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn proof_sites_replace_each_span_once_and_refuse_ambiguity_or_conflicting_destinations() {
    let proof = Proof::from_value(&serde_json::json!({"sites":[
        {"path":"a.rs","target_path":"b.rs","before":"V = 5","after":"V = 6"}]}))
    .unwrap();
    assert_eq!(
        proof
            .apply(Path::new("a.rs"), b"x\nV = 5\ny\n".to_vec())
            .unwrap(),
        b"x\nV = 6\ny\n"
    );
    assert_eq!(
        proof.destination(Path::new("a.rs")).unwrap(),
        Path::new("b.rs")
    );
    assert_eq!(
        proof
            .apply(Path::new("other.rs"), b"V = 5 V = 5".to_vec())
            .unwrap(),
        b"V = 5 V = 5"
    );
    for text in [&b"V = 5 and V = 5"[..], b"nothing"] {
        let error = proof.apply(Path::new("a.rs"), text.to_vec()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("proved source site is ambiguous: a.rs")
        );
    }
    let conflict = Proof::from_value(&serde_json::json!({"sites":[
        {"path":"a.rs","target_path":"b.rs","before":"x","after":"y"},
        {"path":"a.rs","target_path":"c.rs","before":"x","after":"y"}]}));
    assert!(
        conflict
            .err()
            .unwrap()
            .to_string()
            .contains("one source file has conflicting proved destinations")
    );
    assert!(Proof::from_value(&serde_json::json!({})).is_err());
}

#[test]
fn a_validated_proof_masks_only_the_derived_pin_catalog_not_other_reversions() {
    let f = Fixture::new();
    write(
        f.root(),
        "tools/released-migrations.json",
        "{\n  \"k\": 1\n}\n",
    );
    f.git(&["add", "tools"]);
    let base = f.commit("catalog base");
    write(
        f.root(),
        "tools/released-migrations.json",
        "{\n  \"k\": 2\n}\n",
    );
    f.set("new default\nobsolete line\nkeep\n");
    f.git(&["add", "tools"]);
    let source = f.commit("accepted catalog and default");
    write(
        f.root(),
        "tools/released-migrations.json",
        "{\n  \"k\": 1\n}\n",
    );
    f.set("old default\nobsolete line\nkeep\n");
    f.git(&["add", "tools"]);
    let candidate = f.commit("restores both");
    let repo = f.repo();
    let plain = repo.lost_hunks(&base, &source, &candidate).unwrap();
    assert_eq!(plain.len(), 2, "{plain:?}");
    let proof = Proof::from_value(&serde_json::json!({"sites":[]})).unwrap();
    let proved = repo
        .lost_hunks_proved(&base, &source, &candidate, Some(&proof))
        .unwrap();
    assert_eq!(proved.len(), 1, "{proved:?}");
    assert!(
        proved[0].starts_with("crates/fixture/src/lib.rs"),
        "{proved:?}"
    );
}
