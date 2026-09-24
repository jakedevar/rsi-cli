use std::path::PathBuf;
use std::process::Command;

use rsi_common::verification_manifest::{VerificationManifest, parse, validate};

const VALID: &[&str] = &[
    "valid_minimal.md",
    "valid_multi_phase.md",
    "valid_all_buckets.md",
    // S6/D1: manifests carrying `satisfies:`/`covers:` linkage are a strict
    // superset — they validate green exactly like a legacy manifest.
    "linkage_covered.md",
    "linkage_uncovered.md",
];
const INVALID: &[&str] = &[
    "invalid_missing_check.md",
    "invalid_mid_phase.md",
    "invalid_malformed_frontmatter.md",
];
const BIN: &str = env!("CARGO_BIN_EXE_rsi-manifest-validate");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("manifests")
        .join(name)
}

#[test]
fn valid_fixtures_parse_and_validate() {
    for name in VALID {
        let path = fixture(name);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => panic!("read fixture {}: {err}", path.display()),
        };
        let validation = validate(&content);
        assert!(
            validation.valid,
            "{} should be valid: {:?}",
            name, validation.errors
        );
        let manifest = match parse(&content) {
            Ok(manifest) => manifest,
            Err(err) => panic!("{name} should parse: {err}"),
        };
        let json = match serde_json::to_string(&manifest) {
            Ok(json) => json,
            Err(err) => panic!("{name} should serialize: {err}"),
        };
        let decoded: VerificationManifest = match serde_json::from_str(&json) {
            Ok(decoded) => decoded,
            Err(err) => panic!("{name} should deserialize: {err}"),
        };
        assert_eq!(decoded, manifest);
    }
}

#[test]
fn invalid_fixtures_fail_validation() {
    for name in INVALID {
        let path = fixture(name);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => panic!("read fixture {}: {err}", path.display()),
        };
        let validation = validate(&content);
        assert!(!validation.valid, "{name} should be invalid");
    }
}

#[test]
fn validator_cli_accepts_valid_fixtures() {
    for name in VALID {
        let output = match Command::new(BIN).arg(fixture(name)).output() {
            Ok(output) => output,
            Err(err) => panic!("spawn validator for {name}: {err}"),
        };
        assert_eq!(
            output.status.code(),
            Some(0),
            "{name} stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("\"valid\": true"));
    }
}

#[test]
fn validator_cli_rejects_invalid_fixtures() {
    for name in INVALID {
        let output = match Command::new(BIN).arg(fixture(name)).output() {
            Ok(output) => output,
            Err(err) => panic!("spawn validator for {name}: {err}"),
        };
        assert_eq!(
            output.status.code(),
            Some(2),
            "{name} stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("\"valid\": false"));
    }
}

#[test]
fn invalid_fixture_rules_are_specific() {
    let cases = [
        ("invalid_missing_check.md", "DaemonCheck"),
        ("invalid_mid_phase.md", "NoMidPhaseHeadings"),
        ("invalid_malformed_frontmatter.md", "Parse"),
    ];

    for (name, rule) in cases {
        let content = match std::fs::read_to_string(fixture(name)) {
            Ok(content) => content,
            Err(err) => panic!("read fixture {name}: {err}"),
        };
        let validation = validate(&content);
        assert!(
            validation.errors.iter().any(|err| err.rule == rule),
            "{name} should contain rule {rule}: {:?}",
            validation.errors
        );
    }
}
