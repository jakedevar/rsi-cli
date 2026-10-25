//! #676: real source stamps and staged checks, without any fixture compilation.
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rsi_repo_guards::{scratch_git::Policy, source_stamp};

struct Fixture {
    policy: Policy,
    dir: tempfile::TempDir,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new() -> Self {
        let checkout = Path::new(rsi_common::manifest_dir!())
            .join("../..")
            .canonicalize()
            .unwrap();
        let policy = Policy::from_process(&checkout).unwrap();
        let dir = policy.tempdir("rsi-no-build-guard-").unwrap();
        let root = dir.path();
        // Copy exactly the embedded stamp's inputs, not timestamps or artifacts
        // from another session. Cargo supplied this test's native guard binary.
        for source in source_stamp::input_paths(&checkout).unwrap() {
            let relative = source.strip_prefix(&checkout).unwrap();
            let destination = root.join(relative);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(source, destination).unwrap();
        }
        for relative in [
            "scripts/repo-guard",
            "tools/git-hooks/pre-commit",
            "tools/git-hooks/check-rerere",
        ] {
            let destination = root.join(relative);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(checkout.join(relative), destination).unwrap();
        }
        let binary = root.join("target/release").join(if cfg!(windows) {
            "rsi-repo-guard.exe"
        } else {
            "rsi-repo-guard"
        });
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_rsi-repo-guard"), binary).unwrap();
        fs::create_dir_all(root.join("fixture-bin")).unwrap();
        executable(
            &root.join("fixture-bin/cargo"),
            "#!/bin/sh\necho called >> cargo.calls\nexit 99\n",
        );
        let path = std::env::join_paths(std::iter::once(root.join("fixture-bin")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let fixture = Self { policy, dir, path };
        fixture.git(&["init", "-q"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self
            .policy
            .command(self.root(), args)
            .unwrap()
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn shell(&self, script: &str, args: &[&str], role: &str) -> Output {
        let mut command = Command::new("sh");
        command
            .current_dir(self.root())
            .arg(script)
            .args(args)
            .env("PATH", &self.path)
            .env("CARGO_TARGET_DIR", "target")
            .env("RSI_REPO_GUARD_NO_BUILD", "1")
            .env("CLAUDE_AGENT_ROLE", role)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for name in rsi_repo_guards::scratch_git::LOCATION_VARIABLES {
            command.env_remove(name);
        }
        command
            .output()
            .expect("guard shell fixtures require sh on PATH; unsupported without a POSIX shell")
    }

    fn guard(&self, args: &[&str]) -> Output {
        self.shell("scripts/repo-guard", args, "")
    }

    fn assert_no_cargo(&self) {
        assert_eq!(
            fs::read_to_string(self.root().join("cargo.calls")).unwrap_or_default(),
            "",
            "no-build resolution must never invoke Cargo"
        );
    }

    fn stage_blob(&self, contents: &[u8]) {
        let mut child = self
            .policy
            .command(self.root(), &["hash-object", "-w", "--stdin"])
            .unwrap()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(contents).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let oid = String::from_utf8(output.stdout).unwrap();
        self.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{},staged-entry", oid.trim()),
        ]);
    }
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn matching_prebuilt_runs_actual_index_check_through_wrapper_and_hook() {
    let fixture = Fixture::new();
    // Only inert classification data enters Git's object store. The working
    // file is harmless; the guard must judge the staged shebang instead.
    fs::write(fixture.root().join("staged-entry"), "ordinary data\n").unwrap();
    fixture.stage_blob(b"#!/usr/bin/env python3\n");
    for output in [
        fixture.guard(&["new-python", "--staged"]),
        fixture.shell("tools/git-hooks/pre-commit", &[], ""),
    ] {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("staged-entry"), "{error}");
        assert!(error.contains("operator directive #358"), "{error}");
    }
    fixture.stage_blob(b"ordinary data\n");
    for output in [
        fixture.guard(&["new-python", "--staged"]),
        fixture.shell("tools/git-hooks/pre-commit", &[], ""),
    ] {
        assert!(output.status.success(), "{output:?}");
    }
    fixture.assert_no_cargo();
}

#[test]
fn every_pre_commit_guard_inherits_no_build_mode() {
    let fixture = Fixture::new();
    let tree = fixture.git(&["write-tree"]);
    let commit = fixture.git(&[
        "-c",
        "user.name=fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit-tree",
        &tree,
        "-m",
        "fixture",
    ]);
    fixture.git(&["update-ref", "refs/heads/fixture", &commit]);
    fixture.git(&["symbolic-ref", "HEAD", "refs/heads/fixture"]);
    for relative in ["scripts/fixture.sh", "crates/rsi/src/fixture.rs"] {
        let path = fixture.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "// inert fixture\n").unwrap();
        fixture.git(&["add", "--", relative]);
    }
    // Test hook routing independently from resolution (covered above). No
    // guard may lose the selected mode after the role-independent first call.
    executable(
        &fixture.root().join("scripts/repo-guard"),
        "#!/bin/sh\n[ \"$RSI_REPO_GUARD_NO_BUILD\" = 1 ] || exit 98\nprintf '%s\\n' \"$1\" >> guard.calls\n",
    );
    let output = fixture.shell("tools/git-hooks/pre-commit", &[], "pipeline-implement");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(fixture.root().join("guard.calls")).unwrap(),
        "new-python\nidentity-assertions\nportable-shell\ntheme-test-guards\npersonal-paths\noperator-identifiers\n"
    );
    fixture.assert_no_cargo();
}

#[test]
fn external_integration_tests_preserve_real_stamp_but_compiled_inputs_refuse() {
    let fixture = Fixture::new();
    let stamp = source_stamp::stamp(fixture.root()).unwrap();
    let tests = fixture.root().join("crates/rsi-repo-guards/tests");
    fs::create_dir_all(&tests).unwrap();
    fs::write(tests.join("external.rs"), "// integration fixture\n").unwrap();
    assert_eq!(source_stamp::stamp(fixture.root()).unwrap(), stamp);
    assert!(fixture.guard(&["--bin", "new-python"]).status.success());

    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "crates/rsi-repo-guards/Cargo.toml",
        "crates/rsi-repo-guards/build.rs",
        "crates/rsi-repo-guards/src/lib.rs",
        "crates/rsi-repo-guards/src/added.rs",
    ] {
        let path = fixture.root().join(relative);
        let original = fs::read(&path).ok();
        let mut changed = original.clone().unwrap_or_default();
        changed.extend_from_slice(b"\n// changed input\n");
        fs::write(&path, changed).unwrap();
        assert_ne!(source_stamp::stamp(fixture.root()).unwrap(), stamp);
        let output = fixture.guard(&["--bin", "new-python"]);
        assert_eq!(output.status.code(), Some(2), "{relative}: {output:?}");
        assert!(output.stdout.is_empty(), "{relative}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("no Cargo fallback"));
        if let Some(bytes) = original {
            fs::write(path, bytes).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
    }
    assert!(fixture.guard(&["--bin", "new-python"]).status.success());
    fixture.assert_no_cargo();
}
