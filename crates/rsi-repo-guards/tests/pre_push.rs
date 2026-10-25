#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn command_path(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("required fixture command {name}"))
}

fn hook_without_python(guard_exit: i32, missing_binary: bool) -> (Output, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bin = root.join("bin");
    for directory in [
        "bin",
        "tools/git-hooks",
        "scripts",
        "crates/rsi-repo-guards/src",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    // Only these commands are visible to the hook; neither Python spelling
    // can resolve, irrespective of the machine running the test.
    for name in ["dirname", "mktemp", "tar", "rm", "find"] {
        symlink(command_path(name), bin.join(name)).unwrap();
    }
    executable(
        &bin.join("git"),
        r#"#!/bin/sh
case "$1" in
  cat-file)
    case "$3" in
      *:scripts|*:tools|*:crates) exit 0 ;;
      *) exit 1 ;;
    esac ;;
  archive) exec tar -cf - -C "$GUARD_FIXTURE_TREE" scripts tools crates ;;
  ls-tree|diff) exit 0 ;;
  *) exit 1 ;;
esac
"#,
    );
    fs::write(
        root.join("crates/rsi-repo-guards/src/identity_assertions.rs"),
        "// Synthetic guard availability marker.\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/rsi-repo-guards/src/agents_md_size.rs"),
        "// Synthetic second guard availability marker.\n",
    )
    .unwrap();
    executable(
        &root.join("scripts/repo-guard"),
        r#"#!/bin/sh
if [ "$GUARD_FIXTURE_MISSING" = 1 ]; then
  echo 'repo-guard: fixture binary unavailable; refusing to run the guard' >&2
  exit 2
fi
[ "$RSI_REPO_GUARD_NO_BUILD" = 1 ] || exit 98
printf 'resolve:%s\n' "$*" >>"$GUARD_FIXTURE_CALLS"
echo "$GUARD_FIXTURE_TREE/guard-binary"
"#,
    );
    executable(
        &root.join("guard-binary"),
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >>\"$GUARD_FIXTURE_CALLS\"\nexit \"$GUARD_FIXTURE_EXIT\"\n",
    );
    let hook = root.join("tools/git-hooks/pre-push");
    fs::write(&hook, include_str!("../../../tools/git-hooks/pre-push")).unwrap();
    let calls = root.join("calls");
    let mut child = Command::new(command_path("sh"))
        .arg(&hook)
        .current_dir(root)
        .env_clear()
        .env("PATH", &bin)
        .env("TMPDIR", root)
        .env("GUARD_FIXTURE_TREE", root)
        .env("GUARD_FIXTURE_CALLS", &calls)
        .env("GUARD_FIXTURE_EXIT", guard_exit.to_string())
        .env("RSI_REPO_GUARD_NO_BUILD", "1")
        .env(
            "GUARD_FIXTURE_MISSING",
            if missing_binary { "1" } else { "0" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Exercise the actual hook protocol, without publishing a Git ref.
    writeln!(
        child.stdin.take().unwrap(),
        "refs/heads/fixture {} refs/heads/rolling {}",
        "1".repeat(40),
        "0".repeat(40)
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let calls = fs::read_to_string(calls).unwrap_or_default();
    (output, calls)
}

#[test]
fn rust_guard_findings_refuse_the_push_without_python() {
    let (output, calls) = hook_without_python(1, false);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        calls,
        "resolve:--bin identity-assertions\nidentity-assertions\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("check-identity-assertions fails"));
}

#[test]
fn clean_rust_guard_runs_and_passes_without_python() {
    let (output, calls) = hook_without_python(0, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        calls,
        "resolve:--bin identity-assertions\nidentity-assertions\nresolve:--bin agents-md-size\nagents-md-size\n"
    );
}

#[test]
fn missing_rust_guard_binary_refuses_the_push_without_python() {
    let (output, _) = hook_without_python(0, true);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot resolve the repository guards")
    );
}
