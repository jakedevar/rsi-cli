use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rsi_repo_guards::scratch_git::Policy;

const MARKER: &str = "crates/rsi-repo-guards/src/new_python.rs";

struct Fixture {
    policy: Policy,
    dir: tempfile::TempDir,
    base: String,
}

impl Fixture {
    fn new() -> Self {
        let checkout = Path::new(rsi_common::manifest_dir!())
            .join("../..")
            .canonicalize()
            .unwrap();
        let policy = Policy::from_process(&checkout).unwrap();
        let dir = policy.tempdir("rsi-new-python-").unwrap();
        let mut fixture = Self {
            policy,
            dir,
            base: String::new(),
        };
        fixture.git(&["init", "-q"]);
        fixture.add("README");
        fixture.add("fixtures/legacy.py");
        fixture.add(MARKER);
        fixture.base = fixture.commit();
        fixture
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self
            .policy
            .command(
                self.root(),
                &[
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "core.hooksPath=",
                    "-c",
                    "commit.gpgsign=false",
                ],
            )
            .unwrap()
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    // Empty path fixtures only: no Python source is authored or executed.
    fn add(&self, path: &str) {
        self.data(path, b"");
    }

    // Inert bytes for classification: headers/data only, never a Python program.
    fn data(&self, path: &str, data: &[u8]) {
        let absolute = self.root().join(path);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(absolute, data).unwrap();
        self.git(&["add", "--", path]);
    }

    // Build link/object fixtures in Git only, including on Windows. No host
    // symlink is followed and no interpreter is needed or made available.
    fn index_blob(&self, mode: &str, path: &str, bytes: &[u8]) {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = self
            .policy
            .command(self.root(), &["hash-object", "-w", "--stdin"])
            .unwrap()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let oid = String::from_utf8(output.stdout).unwrap();
        self.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{},{path}", oid.trim()),
        ]);
    }

    fn refuses_both(&self, path: &str) {
        refused(&self.guard("new-python", &["--staged"]), path);
        let head = self.commit();
        refused(&self.guard("new-python", &[&self.base, &head]), path);
    }

    fn commit(&self) -> String {
        self.git(&["commit", "-qm", "fixture"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn remove(&self, path: &str) {
        fs::remove_file(self.root().join(path)).unwrap();
        self.git(&["add", "--", path]);
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command.current_dir(self.root());
        for name in rsi_repo_guards::scratch_git::LOCATION_VARIABLES {
            command.env_remove(name);
        }
        command
    }

    fn guard(&self, name: &str, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_rsi-repo-guard"))
            .arg(name)
            .args(args)
            .output()
            .unwrap()
    }
}

fn refused(output: &Output, path: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let detail = String::from_utf8_lossy(&output.stderr);
    for expected in [
        path,
        "operator directive #358",
        "scripts/repo-guard",
        "crates/rsi-repo-guards/README.md",
    ] {
        assert!(detail.contains(expected), "missing {expected:?}: {detail}");
    }
}

#[test]
fn additions_are_refused_in_index_and_exact_committed_diff() {
    let fixture = Fixture::new();
    for path in [
        "new.py",
        "tools/spaced name.py",
        "nested/line\nbreak.py",
        "new.pyw",
        "new.pyi",
        "WINDOWS.PYW",
    ] {
        fixture.add(path);
    }
    let staged = fixture.guard("new-python", &["--staged"]);
    for path in [
        "new.py",
        "new.pyw",
        "new.pyi",
        "WINDOWS.PYW",
        "tools/spaced name.py",
    ] {
        refused(&staged, path);
    }
    let head = fixture.commit();
    // Even an empty/clean working index cannot conceal a committed addition.
    let committed = fixture.guard("new-python", &[&fixture.base, &head]);
    for path in [
        "new.py",
        "new.pyw",
        "new.pyi",
        "WINDOWS.PYW",
        "tools/spaced name.py",
    ] {
        refused(&committed, path);
    }
    assert_eq!(
        fixture.guard("new-python", &["--staged"]).status.code(),
        Some(0)
    );
}

#[test]
fn python_shebang_variants_are_refused_regardless_of_executable_bit() {
    for header in [
        "#!/usr/bin/python\n",
        "#! /opt/bin/python3.13 -I\r\n",
        "#!/usr/bin/python3.13t\n",
        "#!/usr/bin/python3t\n",
        "#!/usr/bin/python3.13d\n",
        "#!/usr/bin/python3.7m\n",
        "#!/opt/bin/python3.13t.exe\n",
        "#!/usr/bin/env python3\n",
        "#!/usr/bin/env python3.13t\n",
        "#!/usr/bin/env python3t\n",
        "#!/usr/bin/env -S python3 -I\n",
        "#!/usr/bin/env -S python3.13t -I\n",
        "#!/usr/bin/env -S python3t -I\n",
        "#!/usr/bin/env --split-string='python3 -I'\n",
        "#!/usr/bin/env -S 'py\"thon\"3 -I'\n",
        "#!/usr/bin/env -Spython3 -I\n",
        "#!/usr/bin/env -i -u UNUSED KEY=value pythonw3\n",
        "#!/opt/bin/pypy3\n",
    ] {
        for executable in [false, true] {
            let fixture = Fixture::new();
            fixture.data("classify-only", header.as_bytes());
            if executable {
                fixture.git(&["update-index", "--chmod=+x", "classify-only"]);
            }
            fixture.refuses_both("classify-only");
        }
    }
}

#[test]
fn ordinary_data_and_other_interpreters_pass_in_both_scans() {
    let fixture = Fixture::new();
    for (index, data) in [
        b"#!/bin/sh\n".as_slice(),
        b"#!/usr/bin/env -S sh -c python3\n",
        b"#!/usr/bin/env -u python3 sh\n",
        b"#!/usr/bin/env NOTE=python3 sh\n",
        b"#!/usr/bin/pythonish\n",
        b"#!/usr/bin/pythonista\n",
        b"#!/usr/bin/env python-config\n",
        b"#!/usr/bin/env -S python3.13t-config -I\n",
        b"#!/usr/bin/pythont\n",
        b"#!/usr/bin/pythond\n",
        b"#!/usr/bin/pythonm\n",
        b"classification: python3\n#!/usr/bin/python3\n",
        b"\0binary\xffdata\n",
        b"#!/usr/bin/env node\n",
    ]
    .iter()
    .enumerate()
    {
        fixture.data(&format!("data-{index}"), data);
    }
    fixture.data("large-data", &vec![0; 100_000]);
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
    let head = fixture.commit();
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &head])
            .status
            .success()
    );
}

#[test]
fn shebang_classification_uses_index_and_commit_not_worktree_or_current_head() {
    let fixture = Fixture::new();
    fixture.data("header", b"#!/usr/bin/env python3\n");
    fs::write(fixture.root().join("header"), b"inert data\n").unwrap();
    refused(&fixture.guard("new-python", &["--staged"]), "header");
    let python_head = fixture.commit();
    fixture.data("header", b"inert data\n");
    let safe_head = fixture.commit();
    refused(
        &fixture.guard("new-python", &[&fixture.base, &python_head]),
        "header",
    );
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &safe_head])
            .status
            .success()
    );
    fs::write(fixture.root().join("header"), b"#!/usr/bin/python3\n").unwrap();
    fixture.add("ordinary");
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
}

#[test]
fn suffixless_rename_and_copy_destinations_are_classified_in_both_scans() {
    for rename in [true, false] {
        let mut fixture = Fixture::new();
        fixture.data("old-header", b"#!/usr/bin/python3\n");
        fixture.base = fixture.commit();
        fixture.data("new-header", b"#!/usr/bin/python3\n");
        if rename {
            fixture.remove("old-header");
        }
        fixture.refuses_both("new-header");
    }
}

#[test]
fn archive_exemption_requires_non_executable_regular_data_without_python_shebang() {
    let fixture = Fixture::new();
    for path in ["thoughts/a.py", "thoughts/a.pyw", "thoughts/a.pyi"] {
        fixture.add(path);
    }
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
    let head = fixture.commit();
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &head])
            .status
            .success()
    );
    for path in ["thoughts/header", "thoughts/header.pyi"] {
        let fixture = Fixture::new();
        fixture.data(path, b"#!/usr/bin/env -S python3 -I\n");
        fixture.refuses_both(path);
    }
    let fixture = Fixture::new();
    fixture.add("thoughts/executable.pyw");
    fixture.git(&["update-index", "--chmod=+x", "thoughts/executable.pyw"]);
    fixture.refuses_both("thoughts/executable.pyw");
}

#[test]
fn symlinks_classify_exact_targets_and_never_use_the_archive_exemption() {
    for (path, target, bytes) in [
        ("alias", "target", b"#!/usr/bin/python3\n".as_slice()),
        ("thoughts/alias", "../target", b"#!/usr/bin/env python3\n"),
        ("alias", "target.pyw", b""),
        ("thoughts/alias.pyi", "../target", b""),
    ] {
        let mut fixture = Fixture::new();
        fixture.data(
            if target.starts_with("../") {
                &target[3..]
            } else {
                target
            },
            bytes,
        );
        fixture.base = fixture.commit();
        fixture.index_blob("120000", path, target.as_bytes());
        // Working-tree data at the link's path cannot conceal its indexed target.
        fixture.data("unrelated", b"inert\n");
        fs::create_dir_all(fixture.root().join(path).parent().unwrap()).unwrap();
        fs::write(fixture.root().join(path), b"not a symlink\n").unwrap();
        fixture.refuses_both(path);
    }
}

#[test]
fn symlink_chains_and_directory_components_resolve_in_candidate_tree() {
    let fixture = Fixture::new();
    fixture.data("dir/target", b"#!/usr/bin/python3\n");
    fixture.index_blob("120000", "dir-alias", b"dir");
    fixture.index_blob("120000", "alias", b"dir-alias/target");
    fixture.refuses_both("alias");

    let fixture = Fixture::new();
    fixture.index_blob("120000", "alias", b"./middle");
    fixture.index_blob("120000", "middle", b"dir/../README");
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
    let head = fixture.commit();
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &head])
            .status
            .success()
    );
}

#[test]
fn unavailable_or_unsafe_link_and_blob_evidence_fails_closed() {
    for target in [
        b"missing".as_slice(),
        b"/external",
        b"../README",
        b"alias",
        b"nul\0target",
    ] {
        let fixture = Fixture::new();
        fixture.index_blob("120000", "alias", target);
        assert_eq!(
            fixture.guard("new-python", &["--staged"]).status.code(),
            Some(2)
        );
        let head = fixture.commit();
        assert_eq!(
            fixture
                .guard("new-python", &[&fixture.base, &head])
                .status
                .code(),
            Some(2)
        );
    }
    let fixture = Fixture::new();
    let tree = fixture.git(&["rev-parse", "HEAD^{tree}"]);
    fixture.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{tree},non-blob"),
    ]);
    assert_eq!(
        fixture.guard("new-python", &["--staged"]).status.code(),
        Some(2)
    );
    let fixture = Fixture::new();
    fixture.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{},missing-blob", "1".repeat(40)),
    ]);
    assert_eq!(
        fixture.guard("new-python", &["--staged"]).status.code(),
        Some(2)
    );
}

#[test]
fn modifications_deletions_and_non_python_additions_still_pass() {
    let fixture = Fixture::new();
    fixture.git(&["update-index", "--chmod=+x", "fixtures/legacy.py"]);
    fixture.add("new.rs");
    let head = fixture.commit();
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &head])
            .status
            .success()
    );
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
    fixture.remove("fixtures/legacy.py");
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
    let head = fixture.commit();
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &head])
            .status
            .success()
    );
}

#[test]
fn renames_and_copies_are_new_destinations_even_when_git_detects_similarity() {
    for (rename, destination) in [
        (true, "tools/destination.py"),
        (false, "tools/destination.py"),
        (true, "tools/destination.pyw"),
        (false, "tools/destination.pyi"),
    ] {
        let fixture = Fixture::new();
        fixture.add(destination);
        if rename {
            fixture.remove("fixtures/legacy.py");
        }
        fixture.refuses_both(destination);
    }
    let fixture = Fixture::new();
    fixture.git(&["mv", "fixtures/legacy.py", "converted.rs"]);
    let head = fixture.commit();
    assert!(
        fixture
            .guard("new-python", &[&fixture.base, &head])
            .status
            .success()
    );
}

#[test]
fn archives_are_exempt_but_new_executable_sources_are_not() {
    let fixture = Fixture::new();
    fixture.add("thoughts/archive/evidence.py");
    fixture.add("thoughts-other/source.py");
    refused(
        &fixture.guard("new-python", &["--staged"]),
        "thoughts-other/source.py",
    );
    fixture.remove("thoughts-other/source.py");
    assert!(fixture.guard("new-python", &["--staged"]).status.success());
    fixture.git(&["update-index", "--chmod=+x", "thoughts/archive/evidence.py"]);
    refused(
        &fixture.guard("new-python", &["--staged"]),
        "thoughts/archive/evidence.py",
    );
}

#[test]
fn initial_index_and_missing_revisions_fail_closed() {
    let fixture = Fixture::new();
    let dir = fixture.policy.tempdir("rsi-new-python-unborn-").unwrap();
    let init = fixture
        .policy
        .command(dir.path(), &["init", "-q"])
        .unwrap()
        .output()
        .unwrap();
    assert!(init.status.success());
    fs::write(dir.path().join("initial.py"), []).unwrap();
    assert!(
        fixture
            .policy
            .command(dir.path(), &["add", "--", "initial.py"])
            .unwrap()
            .status()
            .unwrap()
            .success()
    );
    let mut command = fixture.command(env!("CARGO_BIN_EXE_rsi-repo-guard"));
    let output = command
        .current_dir(dir.path())
        .args(["new-python", "--staged"])
        .output()
        .unwrap();
    refused(&output, "initial.py");
    assert_eq!(
        fixture
            .guard("new-python", &["missing-ref", "HEAD"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        fixture
            .guard("new-python", &["--staged", "HEAD"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        fixture.guard("new-python", &["--unknown"]).status.code(),
        Some(2)
    );
}

#[test]
fn legacy_scoped_static_entry_refuses_additions_and_preserves_clean_checks() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .guard("starter-pins", &[&fixture.base, &fixture.base])
            .status
            .success()
    );
    fixture.add("new.pyi");
    let head = fixture.commit();
    refused(
        &fixture.guard("starter-pins", &[&fixture.base, &head]),
        "new.pyi",
    );
    assert!(fixture.guard("starter-pins", &["--tree"]).status.success());
    let runner = include_str!("../../../scripts/scoped-test");
    assert!(
        runner.contains("\"starter_pins\": [\"scripts/repo-guard\", \"starter-pins\", base, head]")
    );
}

#[cfg(unix)]
mod unix {
    use super::*;
    use rsi_repo_guards::scoped_runner::{
        contracts::Contracts, converged::Converged, orchestration::REQUIRED_GUARDS,
    };
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Stdio;

    fn executable(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn hooks(fixture: &Fixture) {
        // Only these commands are visible to hooks and their guards, never Python.
        let bin = fixture.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in [
            "sh", "git", "dirname", "mktemp", "tar", "rm", "find", "xargs",
        ] {
            let command = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
                .unwrap();
            symlink(command, bin.join(name)).unwrap();
        }
        fs::create_dir_all(fixture.root().join("tools/git-hooks")).unwrap();
        fs::create_dir_all(fixture.root().join("scripts")).unwrap();
        for (name, text) in [
            (
                "pre-commit",
                include_str!("../../../tools/git-hooks/pre-commit"),
            ),
            (
                "pre-push",
                include_str!("../../../tools/git-hooks/pre-push"),
            ),
            (
                "check-rerere",
                include_str!("../../../tools/git-hooks/check-rerere"),
            ),
        ] {
            executable(&fixture.root().join("tools/git-hooks").join(name), text);
        }
        let binary = env!("CARGO_BIN_EXE_rsi-repo-guard").replace('\'', "'\\''");
        executable(
            &fixture.root().join("scripts/repo-guard"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --bin ]; then printf '%s\\n' '{binary}'; else exec '{binary}' \"$@\"; fi\n"
            ),
        );
    }

    fn push(fixture: &Fixture, base: &str, head: &str) -> Output {
        let mut child = fixture
            .command("sh")
            .arg("tools/git-hooks/pre-push")
            .env("PATH", fixture.root().join("bin"))
            .env_remove("PYTHON")
            .env_remove("RSI_SESSION_TOKEN")
            .env_remove("RSI_SESSION_ID")
            .env_remove("CLAUDE_AGENT_ROLE")
            .env_remove("RSI_ROLLING_LANDER")
            .env_remove("RSI_SKIP_STATIC_GUARDS")
            .env_remove("RSI_SKIP_SHARD_CHECK")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "refs/heads/fixture {head} refs/heads/rolling {base}"
        )
        .unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn hooks_refuse_new_paths_without_a_python_interpreter() {
        let fixture = Fixture::new();
        hooks(&fixture);
        fixture.data("new-header", b"#!/usr/bin/env -S python3 -I\n");
        let output = fixture
            .command("sh")
            .arg("tools/git-hooks/pre-commit")
            .env("PATH", fixture.root().join("bin"))
            .env_remove("CLAUDE_AGENT_ROLE")
            .output()
            .unwrap();
        refused(&output, "new-header");
        let head = fixture.commit();
        refused(&push(&fixture, &fixture.base, &head), "new-header");
        fixture.git(&["update-ref", "refs/remotes/origin/rolling", &fixture.base]);
        refused(&push(&fixture, &"0".repeat(40), &head), "new-header");
        fixture.git(&["update-ref", "-d", "refs/remotes/origin/rolling"]);
        let output = push(&fixture, &"0".repeat(40), &head);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("cannot inspect tracked additions without a base")
        );
    }

    #[test]
    fn hooks_allow_existing_modifications_and_deletions() {
        let fixture = Fixture::new();
        hooks(&fixture);
        fixture.git(&["update-index", "--chmod=+x", "fixtures/legacy.py"]);
        assert!(
            fixture
                .command("sh")
                .arg("tools/git-hooks/pre-commit")
                .env("PATH", fixture.root().join("bin"))
                .env_remove("CLAUDE_AGENT_ROLE")
                .status()
                .unwrap()
                .success()
        );
        let head = fixture.commit();
        let output = push(&fixture, &fixture.base, &head);
        assert!(output.status.success(), "{output:?}");
        fixture.remove("fixtures/legacy.py");
        let head = fixture.commit();
        let output = push(&fixture, &fixture.base, &head);
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn symlinked_archives_are_not_silently_exempted() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.root().join("thoughts")).unwrap();
        symlink("../README", fixture.root().join("thoughts/source.py")).unwrap();
        fixture.git(&["add", "--", "thoughts/source.py"]);
        refused(
            &fixture.guard("new-python", &["--staged"]),
            "thoughts/source.py",
        );
    }

    #[test]
    fn rust_scoped_runner_has_a_mandatory_direct_verdict() {
        let fixture = Fixture::new();
        fixture.data("new-header", b"#!/usr/bin/python3\n");
        let head = fixture.commit();
        assert!(REQUIRED_GUARDS.contains(&"new_python"));
        let ctx = rsi_repo_guards::Ctx::from_process();
        let mut contracts =
            Converged::new(&ctx, env!("CARGO_BIN_EXE_rsi-scoped-test").into()).unwrap();
        let value = contracts
            .guard("new_python", fixture.root(), &fixture.base, &head)
            .unwrap();
        assert_eq!(value["status"], "fail", "{value}");
        assert!(
            value["detail"]
                .as_str()
                .unwrap()
                .contains("operator directive #358"),
            "{value}"
        );
        let value = contracts
            .guard("new_python", fixture.root(), &fixture.base, &fixture.base)
            .unwrap();
        assert_eq!(value["status"], "ok", "{value}");
    }
}
