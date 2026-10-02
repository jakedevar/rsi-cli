//! #1016 acceptance 4 at the CLI: a closed or archived owner Issue's signature
//! never classifies or queries, whether the snapshot was exported that way or
//! is a stale file that still lists a since-closed Issue.

use std::path::Path;
use std::process::{Command, Output};

use rsi_common::failure_signature::{FENCE_INFO, digest};

const BIN: &str = env!("CARGO_BIN_EXE_rsi-known-failure");
const TEXT: &str =
    "thread 't::a' (1) panicked at crates/x.rs:1:1:\nboom 42\nnote: run with `RUST_BACKTRACE=1`";
const NEXTEST_LOG: &str = "        FAIL [   0.010s] ( 1/1) pkg t::a\n\n--- STDERR:              pkg t::a ---\nthread 't::a' (1) panicked at crates/x.rs:1:1:\nboom 42\nnote: run with `RUST_BACKTRACE=1`\n\n";

fn issue_body(issue: u64) -> String {
    let record = serde_json::json!({
        "test_id": "t::a",
        "matcher": {"digest": digest(TEXT)},
        "issue": issue,
        "class": "regression",
    });
    format!("```{FENCE_INFO}\n{record}\n```\n")
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run rsi-known-failure")
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(path: &Path, value: &serde_json::Value) {
    std::fs::write(path, serde_json::to_string(value).unwrap()).unwrap();
}

#[test]
fn export_keeps_only_open_unarchived_issues_and_readers_agree() {
    let dir = tempfile::tempdir().unwrap();
    let issues = dir.path().join("issues.json");
    let snapshot = dir.path().join("snapshot.json");
    let log = dir.path().join("nextest.log");
    std::fs::write(&log, NEXTEST_LOG).unwrap();
    write(
        &issues,
        &serde_json::json!([
            {"display_number": 1, "status": "Closed", "archived_at": null, "body": issue_body(1)},
            {"display_number": 2, "status": "Open", "archived_at": "2026-09-30T00:00:00Z", "body": issue_body(2)},
            {"display_number": 3, "status": "Open", "archived_at": null, "body": issue_body(3)},
        ]),
    );
    let exported = run(&[
        "export",
        "--issues-json",
        issues.to_str().unwrap(),
        "--out",
        snapshot.to_str().unwrap(),
    ]);
    assert!(exported.status.success());
    assert!(text(&exported).contains("records=1"), "{}", text(&exported));
    let snap = snapshot.to_str().unwrap();
    let query = run(&["query", "--snapshot", snap, "t::a"]);
    assert_eq!(text(&query).lines().count(), 1);
    assert!(text(&query).contains("\"issue\":3"));
    let classified = run(&[
        "classify",
        "--log",
        log.to_str().unwrap(),
        "--snapshot",
        snap,
    ]);
    assert!(
        text(&classified).contains("KNOWN #3 regression t::a"),
        "{}",
        text(&classified)
    );
}

#[test]
fn a_stale_snapshot_file_cannot_keep_a_closed_issue_alive() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = dir.path().join("stale.json");
    let log = dir.path().join("nextest.log");
    std::fs::write(&log, NEXTEST_LOG).unwrap();
    let record = serde_json::json!({
        "test_id": "t::a",
        "matcher": {"digest": digest(TEXT)},
        "issue": 9,
        "class": "regression",
    });
    write(
        &snapshot,
        &serde_json::json!({
            "schema_version": 1,
            "exported_at": "2020-01-01T00:00:00Z",
            "records": [{"record": record, "issue_status": "Closed"}],
        }),
    );
    let snap = snapshot.to_str().unwrap();
    let query = run(&["query", "--snapshot", snap, "t::a"]);
    assert_eq!(text(&query), "");
    assert!(String::from_utf8_lossy(&query.stderr).contains("h old"));
    let classified = run(&[
        "classify",
        "--log",
        log.to_str().unwrap(),
        "--snapshot",
        snap,
    ]);
    assert!(
        text(&classified).contains("NEW t::a"),
        "{}",
        text(&classified)
    );
}
