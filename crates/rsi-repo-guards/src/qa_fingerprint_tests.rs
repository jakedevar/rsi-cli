//! Ports all five intents in test_rolling_shard_fingerprint.py and
//! test_qa_host_class.py, plus non-ASCII serialization and refusal boundaries.
use super::*;

const RUSTC: &str = "rustc 1.94.0 (000000000 2026-01-01)\nhost: x86_64-unknown-linux-gnu";
const HOST: &str = "linux-x86_64@desktop:a";
const NOISE: &[u8] = b"info: syncing channel updates for 'stable-x86_64-unknown-linux-gnu'\n";

fn fake_fingerprint(noise: &[u8], host: &str, target: Option<&str>) -> Value {
    fingerprint_with(
        &"b".repeat(40),
        "store-01",
        4,
        host,
        target,
        |program, args| {
            let stdout = match (program, args) {
                ("rustc", ["-vV"]) => RUSTC.to_owned(),
                ("cargo", ["--version"]) => "cargo 1.94.0 (000000000 2026-01-01)".into(),
                ("cargo", ["nextest", "--version"]) => "cargo-nextest 0.9.100".into(),
                ("git", ["rev-parse", "--verify", object]) => {
                    assert!(object.starts_with(&format!("{}:", "b".repeat(40))));
                    "a".repeat(40)
                }
                _ => panic!("unexpected probe {program} {args:?}"),
            };
            probe_output(true, stdout.as_bytes(), noise, program)
        },
    )
    .unwrap()
}

#[test]
fn stderr_noise_does_not_change_digest() {
    assert_eq!(
        fake_fingerprint(b"", HOST, None),
        fake_fingerprint(NOISE, HOST, None)
    );
}

#[test]
fn clean_digest_matches_legacy_stdout_plus_stderr() {
    let got = fake_fingerprint(b"", HOST, None);
    // Independently laid out json.dumps(sort_keys=True,separators=(',',':'))
    // oracle from the legacy test's exact input fields (no interpreter run).
    let canonical = concat!(
        "{\"cargo\":\"cargo 1.94.0 (000000000 2026-01-01)\",",
        "\"checker_blob\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"feature\":\"test-shard-store-01\",\"host_class\":\"linux-x86_64@desktop:a\",",
        "\"jobs\":4,\"nextest\":\"cargo-nextest 0.9.100\",",
        "\"nextest_config_blob\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"runner_blob\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
        "\"rustc\":\"rustc 1.94.0 (000000000 2026-01-01)\\nhost: x86_64-unknown-linux-gnu\",",
        "\"target_triple\":\"x86_64-unknown-linux-gnu\",\"test_threads\":4}"
    );
    assert_eq!(legacy_json(&got["inputs"], true), canonical);
    assert_eq!(got["digest"], sha256(canonical));
}

#[test]
fn output_ignores_stderr_and_refuses_failed_probe() {
    assert_eq!(
        probe_output(true, b" cargo 1.94\n", NOISE, "cargo").unwrap(),
        "cargo 1.94"
    );
    assert!(
        probe_output(false, b"cargo 1.94", b"probe broke", "cargo")
            .unwrap_err()
            .contains("probe broke")
    );
    assert!(probe_output(true, &[0xff], b"", "cargo").is_err());
}

#[test]
fn host_identity_survives_shared_class_override() {
    let desktop = host_class_from("linux-x86_64", "Desktop\n", "same-machine-id\n").unwrap();
    assert_eq!(
        desktop,
        host_class_from("linux-x86_64", "desktop", "same-machine-id").unwrap()
    );
    let cloud = host_class_from("linux-x86_64", "cloud", "same-machine-id").unwrap();
    assert_ne!(desktop, cloud);
    assert!(desktop.starts_with("linux-x86_64@desktop:"));
    assert!(cloud.starts_with("linux-x86_64@cloud:"));
    assert_ne!(
        desktop,
        host_class_from("linux-x86_64", "desktop", "other-machine-id").unwrap()
    );
    assert_eq!(
        host_class_from("test", "  ", "").unwrap_err(),
        "QA host has no hostname"
    );
}

#[test]
fn host_class_changes_full_shard_digest() {
    assert_ne!(
        fake_fingerprint(b"", HOST, None)["digest"],
        fake_fingerprint(b"", "linux-x86_64@cloud:b", None)["digest"]
    );
}

#[test]
fn target_override_and_missing_host() {
    let result = fake_fingerprint(b"", HOST, Some("aarch64-apple-darwin"));
    assert_eq!(result["inputs"]["target_triple"], "aarch64-apple-darwin");
    assert_eq!(
        fake_fingerprint(b"", HOST, Some("")),
        fake_fingerprint(b"", HOST, None)
    );
    assert_eq!(
        fingerprint_with("a", "store-01", 4, HOST, None, |_, _| Ok(
            "rustc without host".into()
        ))
        .unwrap_err(),
        "rustc -vV did not report a host target triple"
    );
}

#[test]
fn canonical_json_keeps_ascii_escapes_and_default_spacing() {
    let value = json!({"z": "é😀\u{7f}\u{8}\u{c}\n\r\t\"\\/", "a": [true, 4, null]});
    assert_eq!(
        legacy_json(&value, true),
        "{\"a\":[true,4,null],\"z\":\"\\u00e9\\ud83d\\ude00\\u007f\\b\\f\\n\\r\\t\\\"\\\\/\"}"
    );
    assert_eq!(
        legacy_json(&json!({"z": 1, "a": []}), false),
        "{\"a\": [], \"z\": 1}"
    );
    for (number, expected) in [
        (1e-5, "1e-05"),
        (1e-4, "0.0001"),
        (1e16, "1e+16"),
        (1e15, "1000000000000000.0"),
        (-0.0, "-0.0"),
        (1.5, "1.5"),
    ] {
        assert_eq!(legacy_json(&json!(number), true), expected);
    }
}

#[test]
fn cli_refuses_invalid_oid_shard_and_job_count_before_probes() {
    let ctx = Ctx::new(".", &[]);
    for (sha, shard, jobs, expected) in [
        ("A".repeat(40), "store-01", "4", "--sha must"),
        ("a".repeat(40), "store-1", "4", "--shard must"),
        ("a".repeat(40), "store-01", "65", "--jobs must"),
        ("a".repeat(40), "store-01", "0", "--jobs must"),
    ] {
        let (code, out, err) = crate::testutil::run(
            main,
            &["--sha", &sha, "--shard", shard, "--jobs", jobs],
            &ctx,
        );
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert!(err.contains(expected), "{err}");
    }
}

#[test]
fn cli_probe_failure_has_runtime_exit_code() {
    let scratch = tempfile::TempDir::new().unwrap();
    let ctx = Ctx::new(scratch.path().join("missing"), &[]);
    let (code, out, err) = crate::testutil::run(
        main,
        &[
            "--sha",
            &"a".repeat(40),
            "--shard",
            "store-01",
            "--jobs",
            "4",
        ],
        &ctx,
    );
    assert_eq!(code, 1);
    assert_eq!(out, "");
    assert!(
        err.starts_with("rolling-shard-fingerprint: cannot run "),
        "{err}"
    );
}
