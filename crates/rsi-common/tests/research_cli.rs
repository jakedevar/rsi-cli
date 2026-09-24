//! Integration tests for the `rsi-research-validate` CLI.
//!
//! Locates the binary via `CARGO_BIN_EXE_rsi-research-validate` (cargo sets
//! this for `[[bin]]` targets in the same package), avoiding a separate
//! `cargo run` cold-start per case.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rsi-research-validate"))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn valid_fixture_exits_zero() {
    let output = Command::new(bin())
        .arg(fixture("research_valid.json"))
        .output()
        .expect("spawn validator");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("\"valid\": true"));
    assert!(stdout.contains("\"schema_version\": 1"));
}

#[test]
fn invalid_fixture_exits_two_under_strict() {
    // RSI-021: WordCap, FormatRegex, RangeCheck are strict-only rules.
    // The lenient floor (default) skips them, so this fixture only fails
    // under `--strict`. Lenient-mode behavior on the same fixture is
    // covered by `invalid_fixture_passes_under_lenient_default`.
    let output = Command::new(bin())
        .arg("--strict")
        .arg(fixture("research_invalid.json"))
        .output()
        .expect("spawn validator");
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("\"valid\": false"));
    // Each error must surface on stderr in the human-readable form.
    assert!(stderr.contains("WordCap"));
    assert!(stderr.contains("FormatRegex"));
    assert!(stderr.contains("RangeCheck"));
}

#[test]
fn invalid_fixture_passes_under_lenient_default() {
    // The same fixture that fails strict (WordCap/FormatRegex/RangeCheck
    // violations) passes the lenient floor — those three rules are
    // strict-only. This is the resume-time tolerance gate RSI-021 added.
    let output = Command::new(bin())
        .arg(fixture("research_invalid.json"))
        .output()
        .expect("spawn validator");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0 (lenient pass), stdout: {}",
        stdout
    );
    assert!(stdout.contains("\"valid\": true"));
    assert!(stdout.contains("\"mode\": \"lenient\""));
}

#[test]
fn missing_file_exits_one() {
    let mut path = std::env::temp_dir();
    path.push("rsi-research-validate-does-not-exist-001.json");
    let _ = std::fs::remove_file(&path);

    let output = Command::new(bin())
        .arg(&path)
        .output()
        .expect("spawn validator");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("I/O error"));
}

#[test]
fn malformed_json_exits_two_with_parse_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("malformed.json");
    std::fs::write(&path, "{not valid").expect("write malformed");

    let output = Command::new(bin())
        .arg(&path)
        .output()
        .expect("spawn validator");
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("\"rule\": \"Parse\""));
    assert!(stderr.contains("Parse"));
}

#[test]
fn future_schema_version_rejected() {
    let output = Command::new(bin())
        .arg(fixture("research_valid.json"))
        .arg("--schema-version")
        .arg("99")
        .output()
        .expect("spawn validator");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("schema v2"));
}

#[test]
fn unknown_flag_exits_two() {
    let output = Command::new(bin())
        .arg("--frobnicate")
        .output()
        .expect("spawn validator");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn no_args_exits_two() {
    let output = Command::new(bin()).output().expect("spawn validator");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn v2_unique_ids_fixture_exits_zero_under_strict() {
    // RSI #382: a v2 doc whose findings carry distinct provenance ids
    // validates green under `--strict` — the new UniqueID rule must not
    // reject legitimate unique keys.
    let output = Command::new(bin())
        .arg("--strict")
        .arg(fixture("research_v2_unique.json"))
        .output()
        .expect("spawn validator");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("\"valid\": true"));
    assert!(stdout.contains("\"schema_version\": 2"));
    assert!(stdout.contains("\"mode\": \"strict\""));
}

#[test]
fn v2_duplicate_ids_fixture_exits_two_under_strict() {
    // RSI #382: two findings sharing the same present F-001 id must fail
    // strict v2 with a bounded field-specific UniqueID error.
    let output = Command::new(bin())
        .arg("--strict")
        .arg(fixture("research_v2_duplicate.json"))
        .output()
        .expect("spawn validator");
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("\"valid\": false"));
    assert!(stdout.contains("\"rule\": \"UniqueID\""));
    assert!(stdout.contains("\"field\": \"findings[1].id\""));
    assert!(stderr.contains("UniqueID"));
    assert!(stderr.contains("findings[1].id"));
    assert!(stderr.contains("F-001"));
}

#[test]
fn v2_duplicate_ids_fixture_passes_under_lenient_default() {
    // Legacy tolerance: the duplicate-id rule is strict-only (like WordCap /
    // FormatRegex / RangeCheck), so the same fixture passes the default
    // lenient floor at resume time.
    let output = Command::new(bin())
        .arg(fixture("research_v2_duplicate.json"))
        .output()
        .expect("spawn validator");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0 (lenient pass), stdout: {}",
        stdout
    );
    assert!(stdout.contains("\"valid\": true"));
    assert!(stdout.contains("\"mode\": \"lenient\""));
}
