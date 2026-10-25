//! Executable contract of the native scoped-test command. The staged binary has
//! no sibling contracts, so a complete run is never green: every case here
//! asserts the red, fail-closed shape and the retained command-line contract.
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const PREFIX: &str = "SCOPED_TEST_RECEIPT ";

fn scoped(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rsi-scoped-test"));
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default());
    command
}
fn receipt(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(PREFIX))
        .unwrap_or_else(|| panic!("no receipt in {text}"));
    serde_json::from_str(line).unwrap()
}
fn git(dir: &Path, args: &[&str]) {
    let done = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        done.status.success(),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
}
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-qm", "one"]);
    dir
}

#[test]
fn usage_errors_exit_two_with_argparse_style_text() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["--runner", "bogus"],
        vec!["--runtime-max-sec", "0"],
        vec!["unexpected"],
    ] {
        let done = scoped(dir.path(), &args).output().unwrap();
        assert_eq!(done.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&done.stderr).contains("scoped-test: error:"));
    }
}

#[test]
fn a_dry_run_and_an_explicit_filter_run_are_red_until_every_sibling_contract_exists() {
    let dir = repository();
    // The runner derives in process (#426's API, converged contracts): a fixture without the
    // repository's shard inventory fails the derivation, a named red refusal that carries the
    // reasons (it used to be `incomplete_contract` while the derivation was staged).
    let done = scoped(dir.path(), &["--base", "HEAD", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(done.status.code(), Some(2));
    let value = receipt(&done);
    assert_eq!(value["ok"], false);
    assert_eq!(value["exit_code"], 2);
    assert_eq!(value["failure_class"], "derive_failed");
    assert!(
        value["derive_failure"]
            .as_array()
            .is_some_and(|reasons| !reasons.is_empty()),
        "{value}"
    );
    assert_eq!(value["tool"]["runner"], "rust");
    // Explicit filters need the shard inventory first: #362 supplies it, and an unreadable
    // manifest is a named, red refusal.
    let done = scoped(
        dir.path(),
        &["--base", "HEAD", "--filter", "rsi=test(=one)"],
    )
    .output()
    .unwrap();
    assert_eq!(done.status.code(), Some(2));
    let value = receipt(&done);
    assert_eq!(value["ok"], false);
    assert_eq!(value["exit_code"], 2);
    assert_eq!(value["failure_class"], "incomplete_contract");
    assert!(
        ["#362", "#426"].contains(&value["incomplete"]["owner"].as_str().unwrap()),
        "{value}"
    );
    assert_eq!(value["tool"]["runner"], "rust");
}

#[test]
fn unresolvable_and_dirty_sources_are_refused_before_any_lane() {
    let dir = repository();
    let done = scoped(dir.path(), &["--base", "no-such-ref"])
        .output()
        .unwrap();
    assert_eq!(done.status.code(), Some(2));
    assert_eq!(receipt(&done)["ok"], false);
    std::fs::write(dir.path().join("dirty.txt"), "x\n").unwrap();
    let done = scoped(dir.path(), &["--base", "HEAD"]).output().unwrap();
    assert_eq!(done.status.code(), Some(2));
    assert_eq!(receipt(&done)["failure_class"], "job_source_moved");
}

#[test]
fn warning_baseline_from_prints_the_measured_packages_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("job.log");
    std::fs::write(
        &log,
        "noise\nSCOPED_WARNINGS {\"measured\":{\"pkg\":[\"lib\"]},\"warnings\":{\"pkg\":{\"k|1|m\":[\"a\"]}},\"unattributed\":0,\"degraded\":[]}\n",
    )
    .unwrap();
    let done = scoped(
        dir.path(),
        &["--warning-baseline-from", log.to_str().unwrap()],
    )
    .output()
    .unwrap();
    assert_eq!(done.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&done.stdout).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"schema":1,"packages":{"pkg":{"configs":["lib"],"warnings":{"k|1|m":1}}}})
    );
}

#[test]
fn package_execution_requires_a_held_admission_and_the_holder_attests_only_build_knobs() {
    let dir = tempfile::tempdir().unwrap();
    let done = scoped(dir.path(), &["--execute-plan", "missing.json"])
        .output()
        .unwrap();
    assert_eq!(done.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&done.stderr).contains("requires a held cargo-slot admission"));

    let held = scoped(dir.path(), &["--hold-admission"])
        .env("RSI_CARGO_SLOT_HELD", "1")
        .env("UNRELATED_CREDENTIAL", "secret")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(held.status.code(), Some(0));
    let text = String::from_utf8_lossy(&held.stdout);
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("SCOPED_SLOT_ENV "))
        .unwrap();
    let attested: Value = serde_json::from_str(line).unwrap();
    assert_eq!(attested["RSI_CARGO_SLOT_HELD"], "1");
    assert!(attested.get("UNRELATED_CREDENTIAL").is_none());
}

#[test]
fn the_runner_is_its_own_pid_isolation_helper() {
    let dir = tempfile::tempdir().unwrap();
    let done = scoped(dir.path(), &["pid-isolate", "--status"])
        .env("RSI_PID_ISOLATION", "off")
        .output()
        .unwrap();
    assert_eq!(done.status.code(), Some(3));
    let value: Value = serde_json::from_slice(&done.stdout).unwrap();
    assert_eq!(value["mode"], "unavailable");
}
