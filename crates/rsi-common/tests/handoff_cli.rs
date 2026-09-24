//! CLI integration tests for `rsi-handoff-validate`.
//!
//! Cargo provides the path to the built binary via the
//! `CARGO_BIN_EXE_<name>` environment variable at integration-test
//! compile time, so no new deps are needed.
//!
//! Coverage:
//! - Valid synthetic handoff → exit 0; stdout JSON `valid: true`.
//! - Missing INA strict → exit 2; stdout JSON `valid: false`.
//! - Missing INA lenient → exit 0.
//! - Bad path → exit 1.
//! - Unknown frontmatter key → exit 2.
//! - --schema-version mismatch → exit 1.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rsi-handoff-validate");

fn good_handoff() -> &'static str {
    r#"---
date: "2026-04-25"
researcher: jake
git_commit: abc123
branch: main
repository: rsi
topic: "CLI test"
tags: [test]
status: complete
last_updated: "2026-04-25"
last_updated_by: jake
type: implementation_strategy
schema_version: 1
---

## Immediate Next Action

Run cargo test and confirm all CLI exit codes behave correctly.

## Original Request

Verify the CLI integration test fixtures pass strict mode end-to-end so that downstream skill markdown can rely on exit-code contracts.

## Task(s)

- CLI test fixture: done

## Critical References

- crates/rsi-common/tests/handoff_cli.rs:1
- crates/rsi-common/src/bin/rsi-handoff-validate.rs:1

## Recent Changes

- crates/rsi-common/tests/handoff_cli.rs — new integration test

## Learnings

- CARGO_BIN_EXE_<name> avoids needing escargot for test binaries.

## Artifacts

- crates/rsi-common/tests/handoff_cli.rs

## Action Items & Next Steps

- Wire skill markdown after CLI suite is green.
"#
}

fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("rsi-handoff-validate-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn valid_handoff_strict_exits_zero() {
    let path = write_temp("good.md", good_handoff());
    let out = Command::new(BIN)
        .arg("--strict")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"valid\":true"));
    assert!(stdout.contains("\"errors\":[]"));
}

#[test]
fn valid_handoff_lenient_default_exits_zero() {
    let path = write_temp("good_lenient.md", good_handoff());
    let out = Command::new(BIN).arg(&path).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"mode\":\"lenient\""));
}

#[test]
fn missing_ina_strict_exits_two() {
    let doc = good_handoff().replace(
        "## Immediate Next Action\n\nRun cargo test and confirm all CLI exit codes behave correctly.\n\n",
        "",
    );
    let path = write_temp("missing_ina.md", &doc);
    let out = Command::new(BIN)
        .arg("--strict")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"valid\":false"));
    assert!(stdout.contains("Immediate Next Action"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Immediate Next Action"));
    assert!(stderr.contains("Presence"));
}

#[test]
fn missing_ina_lenient_exits_zero() {
    let doc = good_handoff().replace(
        "## Immediate Next Action\n\nRun cargo test and confirm all CLI exit codes behave correctly.\n\n",
        "",
    );
    let path = write_temp("missing_ina_lenient.md", &doc);
    let out = Command::new(BIN).arg(&path).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "lenient should pass; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn missing_path_exits_one() {
    let out = Command::new(BIN)
        .arg("/nonexistent/path/that/does/not/exist.md")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("I/O error"));
}

#[test]
fn unknown_frontmatter_key_strict_exits_two() {
    let doc = good_handoff().replace("schema_version: 1", "schema_version: 1\nrogue_field: oops");
    let path = write_temp("rogue_key_strict.md", &doc);
    let out = Command::new(BIN)
        .arg("--strict")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"valid\":false"));
}

#[test]
fn unknown_frontmatter_key_lenient_exits_zero() {
    let doc = good_handoff().replace("schema_version: 1", "schema_version: 1\nrogue_field: oops");
    let path = write_temp("rogue_key_lenient.md", &doc);
    let out = Command::new(BIN).arg(&path).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "lenient must tolerate extra keys; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn schema_version_flag_mismatch_exits_one() {
    let path = write_temp("ver.md", good_handoff());
    let out = Command::new(BIN)
        .arg("--schema-version")
        .arg("99")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn no_args_exits_one() {
    let out = Command::new(BIN).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

/// Round-trip the JSON output through serde to confirm it deserializes
/// back to the same shape — guards against accidental schema breakage.
#[test]
fn stdout_json_round_trips() {
    let path = write_temp("roundtrip.md", good_handoff());
    let out = Command::new(BIN).arg(&path).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout must be valid JSON");
    assert_eq!(parsed["valid"], serde_json::json!(true));
    assert_eq!(parsed["schema_version"], serde_json::json!(1));
    assert_eq!(parsed["mode"], serde_json::json!("lenient"));
    assert!(parsed["errors"].is_array());
}
