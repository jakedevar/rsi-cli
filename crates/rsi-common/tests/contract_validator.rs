use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rsi_common::agent_contract::{
    ContractError, cross_stage_verify_coverage, parse_pipeline_handoff,
};
use rsi_common::verification_manifest::parse as parse_manifest;

const BIN: &str = env!("CARGO_BIN_EXE_rsi-contract-validate");

fn manifest_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("manifests")
        .join(name)
}

fn manifest_fixture(name: &str) -> String {
    let path = manifest_fixture_path(name);
    match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => panic!("read fixture {}: {err}", path.display()),
    }
}

/// A VERIFY handoff that DECLARES it must satisfy plan finding `F-002`.
const VERIFY_DECLARING_F002: &str = "PIPELINE HANDOFF — VERIFY:\n\
     =============================\n\
     Manifest: /tmp/verification.md\n\
     Daemon checks: 2/2\n\
     satisfies: F-002\n\
     Status: complete\n";

fn run_contract(input: &str) -> std::process::Output {
    run_contract_args(input, &["V1.1"])
}

/// Spawn the built validator with `args` (positional ticket + flags), pipe
/// `input` to stdin, and capture the process output. Used by the binary-level
/// end-to-end assertions so the exit code we check is the *process's* code.
fn run_contract_args(input: &str, args: &[&str]) -> std::process::Output {
    let mut child = match Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => panic!("spawn contract validator: {err}"),
    };
    {
        let Some(stdin) = child.stdin.as_mut() else {
            panic!("validator stdin should be piped");
        };
        if let Err(err) = std::io::Write::write_all(stdin, input.as_bytes()) {
            panic!("write validator stdin: {err}");
        }
    }
    match child.wait_with_output() {
        Ok(output) => output,
        Err(err) => panic!("wait for contract validator: {err}"),
    }
}

/// Each admission test owns its paths, including deterministic missing,
/// unreadable (a directory), and malformed manifests. No permission tricks or
/// assumptions about shared /tmp files are needed.
struct ManifestFixtures {
    _dir: tempfile::TempDir,
    covered: PathBuf,
    uncovered: PathBuf,
    missing: PathBuf,
    unreadable: PathBuf,
    malformed: PathBuf,
}

impl ManifestFixtures {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap_or_else(|err| panic!("create fixtures: {err}"));
        let covered = dir.path().join("covered.md");
        let uncovered = dir.path().join("uncovered.md");
        let missing = dir.path().join("missing.md");
        let unreadable = dir.path().join("directory.md");
        let malformed = dir.path().join("malformed.md");
        for (path, content) in [
            (&covered, manifest_fixture("linkage_covered.md")),
            (&uncovered, manifest_fixture("linkage_uncovered.md")),
            (&malformed, "not a verification manifest\n".to_string()),
        ] {
            std::fs::write(path, content)
                .unwrap_or_else(|err| panic!("write fixture {}: {err}", path.display()));
        }
        std::fs::create_dir(&unreadable)
            .unwrap_or_else(|err| panic!("create unreadable fixture: {err}"));
        Self {
            _dir: dir,
            covered,
            uncovered,
            missing,
            unreadable,
            malformed,
        }
    }
}

fn linked_verify_handoff(manifest: &Path) -> String {
    format!(
        "PIPELINE HANDOFF — VERIFY:\n\
         Manifest path: {}\n\
         Daemon checks: 2/2\n\
         satisfies: F-002\n\
         Status: complete\n",
        manifest.display()
    )
}

fn run_manifest_gate(input: &str, explicit: Option<&Path>, strict: bool) -> std::process::Output {
    let mut args = vec!["RSI-242"];
    if strict {
        args.push("--strict-v2");
    }
    let explicit = explicit.map(|path| path.to_string_lossy());
    if let Some(path) = explicit.as_deref() {
        args.extend(["--manifest", path]);
    }
    run_contract_args(input, &args)
}

fn assert_process_exit(output: &std::process::Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_required_manifest_failure(
    implicit: &Path,
    explicit: Option<&Path>,
    strict: bool,
    diagnostic: &str,
) {
    let output = run_manifest_gate(&linked_verify_handoff(implicit), explicit, strict);
    assert_process_exit(&output, 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "cross-stage gate",
        diagnostic,
        "required",
        if explicit.is_some() {
            "--manifest"
        } else {
            "--strict-v2"
        },
        &explicit.unwrap_or(implicit).to_string_lossy(),
    ] {
        assert!(stderr.contains(expected), "stderr: {stderr}");
    }
    // Admission failures preserve the already-emitted handoff JSON. It is
    // the final process status, not a parsed envelope, that reports failure.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"stage\": \"VERIFY\""), "stdout: {stdout}");
    assert!(stdout.contains("F-002"), "stdout: {stdout}");
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap_or_else(|err| panic!("expected one handoff JSON envelope: {err}"));
}

#[test]
fn implementation_handoff_requires_manifest_path() {
    let output = run_contract(
        "PIPELINE HANDOFF — IMPLEMENTATION:\n\
         =============================\n\
         Implementation document: /tmp/plan.md\n\
         Status: complete\n",
    );
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("manifest_path"));
}

#[test]
fn strict_v2_binary_canonicalizes_observed_aliases_and_emits_warnings() {
    for (input, doc_path, aliases) in [
        (
            "PIPELINE HANDOFF — PLAN:\n\
             plan_path: /tmp/plan.md\n\
             worker_status: complete\n",
            "/tmp/plan.md",
            vec![("plan_path", "doc_path"), ("worker_status", "status")],
        ),
        (
            "PIPELINE HANDOFF — REVIEW:\n\
             review_path: /tmp/review.md\n\
             status: complete\n",
            "/tmp/review.md",
            vec![("review_path", "doc_path")],
        ),
    ] {
        let output = run_contract_args(input, &["NO-IDLE", "--strict-v2"]);
        assert_process_exit(&output, 0);
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|err| panic!("expected validator JSON result: {err}"));
        assert_eq!(result["doc_path"], doc_path);
        assert_eq!(result["strict_status"], "complete");
        let warnings = result["warnings"].as_array().expect("warning array");
        assert_eq!(warnings.len(), aliases.len());
        for (warning, (alias, canonical_key)) in warnings.iter().zip(aliases) {
            assert_eq!(warning["alias"], alias);
            assert_eq!(warning["canonical_key"], canonical_key);
        }
    }
}

#[test]
fn implementation_handoff_rejects_inline_manual_checklist() {
    let output = run_contract(
        "PIPELINE HANDOFF — IMPLEMENTATION:\n\
         =============================\n\
         Implementation document: /tmp/plan.md\n\
         Status: complete\n\
         Manual verification checklist:\n\
           - [ ] verify daemon emitted log line\n",
    );
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("manifest_path"));
}

#[test]
fn implementation_handoff_accepts_manifest_path() {
    let output = run_contract(
        "PIPELINE HANDOFF — IMPLEMENTATION:\n\
         =============================\n\
         Implementation document: /tmp/plan.md\n\
         Manifest path: /tmp/verification.md\n\
         Status: complete\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"stage\": \"IMPLEMENTATION\""));
    assert!(stdout.contains("\"manifest_path\": \"/tmp/verification.md\""));
}

#[test]
fn verify_handoff_accepts_daemon_check_schema() {
    let output = run_contract(
        "PIPELINE HANDOFF — VERIFY:\n\
         =============================\n\
         Manifest: /tmp/verification.md\n\
         Daemon checks: 2/2\n\
         Status: complete\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"stage\": \"VERIFY\""));
    assert!(stdout.contains("\"daemon_checks_passed\": 2"));
    assert!(stdout.contains("\"total\": 2"));
}

#[test]
fn strict_v2_rejects_missing_status_and_untyped_blocker() {
    let missing_status = run_contract_args(
        "PIPELINE HANDOFF — RESEARCH:\nResearch document: /tmp/research.md\n",
        &["NO-IDLE", "--strict-v2"],
    );
    assert_eq!(missing_status.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing_status.stdout).contains("status"));

    let untyped = run_contract_args(
        "PIPELINE HANDOFF — IMPLEMENTATION:\n\
         Implementation document: /tmp/plan.md\n\
         Manifest path: /tmp/verification.md\n\
         Status: blocked\n\
         Blocker: iteration budget ended\n",
        &["NO-IDLE", "--strict-v2"],
    );
    assert_eq!(untyped.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&untyped.stdout).contains("blocker_class"));
}

#[test]
fn strict_v2_accepts_typed_technical_impasse() {
    let output = run_contract_args(
        "PIPELINE HANDOFF — IMPLEMENTATION:\n\
         Implementation document: /tmp/plan.md\n\
         Manifest path: /tmp/verification.md\n\
         Status: blocked\n\
         Blocker: compiler crashes on the same isolated fixture\n\
         Blocker class: technical_impasse\n\
         Blocker evidence: three foreground attempts exit with the same signal\n",
        &["NO-IDLE", "--strict-v2"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn strict_v2_accepts_evidenced_partial_without_human_gate_class() {
    let output = run_contract_args(
        "PIPELINE HANDOFF — RESEARCH:\n\
         Research document: /tmp/research.md\n\
         Status: partial\n\
         Blocker: return budget ended before the final source\n\
         Blocker evidence: one named source remains unread\n",
        &["NO-IDLE", "--strict-v2"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn orchestration_outcome_binary_enforces_no_idle_relationships() {
    let invalid = run_contract_args(
        "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":true,\"continuation_state\":\"queue_exhausted\",\"evidence\":\"work remains\"}\n",
        &["--orchestration-outcome"],
    );
    assert_eq!(invalid.status.code(), Some(2));

    let valid = run_contract_args(
        "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":true,\"continuation_state\":\"child_watch\",\"continuation_job_id\":\"00000000-0000-0000-0000-00000000002a\",\"evidence\":\"enabled scheduled-job row verified\"}\n",
        &["--orchestration-outcome"],
    );
    assert_eq!(
        valid.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&valid.stdout),
        String::from_utf8_lossy(&valid.stderr)
    );

    let malformed = run_contract_args(
        "orchestration_outcome_v1: not-json\nMode: program\n",
        &["--orchestration-outcome"],
    );
    assert_eq!(malformed.status.code(), Some(2));

    let duplicate_carrier = "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":false,\"continuation_state\":\"queue_exhausted\",\"evidence\":\"queue empty\"}\n";
    let duplicate = run_contract_args(
        &format!("{duplicate_carrier}{duplicate_carrier}"),
        &["--orchestration-outcome"],
    );
    assert_eq!(duplicate.status.code(), Some(2));

    let slice = run_contract_args(
        "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"slice\",\"next_slice_ready\":false,\"continuation_state\":\"queue_exhausted\",\"evidence\":\"returned to program parent\"}\n",
        &["--orchestration-outcome"],
    );
    assert_eq!(
        slice.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&slice.stdout),
        String::from_utf8_lossy(&slice.stderr)
    );
}

// ----- Cross-stage VERIFY coverage gate (S6/D1) — the PINNING TEST -----
//
// This is the "research→plan→impl drift is a hard gate" assertion. A VERIFY
// handoff declares the plan/research finding it must satisfy; the ticket's
// verification manifest is the source of truth for what the implementation
// actually covered. When a planned finding (`F-002`) is NOT covered by any
// manifest item, the cross-stage pass MUST fail. The same handoff against a
// manifest that covers `F-002` MUST pass. If this ever stops failing on the
// uncovered fixture, the drift gate is broken.

#[test]
fn cross_stage_gate_fails_on_uncovered_plan_item() {
    let manifest = match parse_manifest(&manifest_fixture("linkage_uncovered.md")) {
        Ok(manifest) => manifest,
        Err(err) => panic!("linkage_uncovered.md should parse: {err:?}"),
    };
    let handoff = match parse_pipeline_handoff(VERIFY_DECLARING_F002, "S6") {
        Ok(handoff) => handoff,
        Err(err) => panic!("verify handoff should parse: {err:?}"),
    };
    // The uncovered fixture links only F-001 and F-003 — never F-002.
    match cross_stage_verify_coverage(&handoff, &manifest) {
        Err(ContractError::UncoveredLinkage { keys }) => assert_eq!(keys, vec!["F-002"]),
        Ok(()) => panic!("drift gate must FAIL when a planned finding is uncovered"),
        Err(other) => panic!("expected UncoveredLinkage, got {other:?}"),
    }
}

#[test]
fn cross_stage_gate_passes_on_fully_covered_plan_item() {
    let manifest = match parse_manifest(&manifest_fixture("linkage_covered.md")) {
        Ok(manifest) => manifest,
        Err(err) => panic!("linkage_covered.md should parse: {err:?}"),
    };
    let handoff = match parse_pipeline_handoff(VERIFY_DECLARING_F002, "S6") {
        Ok(handoff) => handoff,
        Err(err) => panic!("verify handoff should parse: {err:?}"),
    };
    // The covered fixture links F-002 (via `covers:`), so the gate passes.
    assert!(
        cross_stage_verify_coverage(&handoff, &manifest).is_ok(),
        "fully-covered plan item must PASS the drift gate"
    );
}

// ----- Binary-level end-to-end: the ARMED gate fires through the process -----
//
// The two tests above prove the *library* gate. These prove the *binary* wires
// it end-to-end: a VERIFY handoff that declares `satisfies: F-002` piped to the
// real `rsi-contract-validate` process, with `--manifest <fixture>`, must exit
// 2 against the uncovered fixture and 0 against the covered one. This is the
// shipped enforcement path the master invokes at Step 3.5.

#[test]
fn binary_cross_stage_gate_exits_2_on_uncovered_manifest() {
    let manifest = manifest_fixture_path("linkage_uncovered.md");
    let output = run_contract_args(
        VERIFY_DECLARING_F002,
        &["S6", "--manifest", &manifest.to_string_lossy()],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "uncovered manifest must FAIL the armed gate\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The serialized ContractError names the uncovered key.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("uncovered_linkage"), "stdout: {stdout}");
    assert!(stdout.contains("F-002"), "stdout: {stdout}");
}

#[test]
fn binary_cross_stage_gate_exits_0_on_covered_manifest() {
    let manifest = manifest_fixture_path("linkage_covered.md");
    let output = run_contract_args(
        VERIFY_DECLARING_F002,
        &["S6", "--manifest", &manifest.to_string_lossy()],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "covered manifest must PASS the armed gate\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn binary_cross_stage_gate_dormant_without_manifest() {
    // Backward-compat: a VERIFY handoff that declares linkage but resolves no
    // manifest in legacy mode (no --manifest or --strict-v2) still exits 0.
    let fixtures = ManifestFixtures::new();
    let output = run_manifest_gate(&linked_verify_handoff(&fixtures.missing), None, false);
    assert_process_exit(&output, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unreadable"), "stderr: {stderr}");
    assert!(
        stderr.contains("skipping coverage check"),
        "stderr: {stderr}"
    );
}

#[test]
fn binary_required_manifest_legacy_explicit_missing() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(
        &fixtures.covered,
        Some(&fixtures.missing),
        false,
        "unreadable",
    );
}

#[test]
fn binary_required_manifest_strict_explicit_missing() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(
        &fixtures.covered,
        Some(&fixtures.missing),
        true,
        "unreadable",
    );
}

#[test]
fn binary_required_manifest_legacy_explicit_unreadable() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(
        &fixtures.covered,
        Some(&fixtures.unreadable),
        false,
        "unreadable",
    );
}

#[test]
fn binary_required_manifest_strict_explicit_unreadable() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(
        &fixtures.covered,
        Some(&fixtures.unreadable),
        true,
        "unreadable",
    );
}

#[test]
fn binary_required_manifest_legacy_explicit_malformed() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(
        &fixtures.covered,
        Some(&fixtures.malformed),
        false,
        "does not parse",
    );
}

#[test]
fn binary_required_manifest_strict_explicit_malformed() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(
        &fixtures.covered,
        Some(&fixtures.malformed),
        true,
        "does not parse",
    );
}

#[test]
fn binary_required_manifest_strict_implicit_missing() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(&fixtures.missing, None, true, "unreadable");
}

#[test]
fn binary_required_manifest_strict_implicit_unreadable() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(&fixtures.unreadable, None, true, "unreadable");
}

#[test]
fn binary_required_manifest_strict_implicit_malformed() {
    let fixtures = ManifestFixtures::new();
    assert_required_manifest_failure(&fixtures.malformed, None, true, "does not parse");
}

#[test]
fn binary_cross_stage_gate_legacy_skips_malformed_implicit_manifest() {
    let fixtures = ManifestFixtures::new();
    let output = run_manifest_gate(&linked_verify_handoff(&fixtures.malformed), None, false);
    assert_process_exit(&output, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not parse"), "stderr: {stderr}");
    assert!(
        stderr.contains("skipping coverage check"),
        "stderr: {stderr}"
    );
}

#[test]
fn binary_cross_stage_gate_legacy_skips_unreadable_implicit_manifest() {
    let fixtures = ManifestFixtures::new();
    let output = run_manifest_gate(&linked_verify_handoff(&fixtures.unreadable), None, false);
    assert_process_exit(&output, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unreadable"), "stderr: {stderr}");
    assert!(
        stderr.contains("skipping coverage check"),
        "stderr: {stderr}"
    );
}

#[test]
fn binary_cross_stage_gate_strict_covered_and_uncovered_controls() {
    let fixtures = ManifestFixtures::new();
    for explicit in [false, true] {
        for (manifest, expected) in [(&fixtures.covered, 0), (&fixtures.uncovered, 2)] {
            let output = run_manifest_gate(
                &linked_verify_handoff(manifest),
                explicit.then_some(manifest.as_path()),
                true,
            );
            assert_process_exit(&output, expected);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let values = serde_json::Deserializer::from_slice(&output.stdout)
                .into_iter::<serde_json::Value>()
                .collect::<Result<Vec<_>, _>>()
                .unwrap_or_else(|err| panic!("expected JSON envelopes: {err}\nstdout: {stdout}"));
            assert_eq!(values.len(), if expected == 2 { 2 } else { 1 });
            assert_eq!(values[0]["stage"], "VERIFY", "stdout: {stdout}");
            assert_eq!(values[0]["strict_status"], "complete", "stdout: {stdout}");
            if expected == 2 {
                let error = &values[1];
                assert_eq!(error["kind"], "uncovered_linkage", "stdout: {stdout}");
                assert_eq!(error["keys"], serde_json::json!(["F-002"]));
            }
        }
    }
}

#[test]
fn binary_cross_stage_gate_explicit_covered_overrides_bad_implicit_manifest() {
    let fixtures = ManifestFixtures::new();
    for strict in [false, true] {
        for implicit in [&fixtures.missing, &fixtures.unreadable, &fixtures.malformed] {
            let output = run_manifest_gate(
                &linked_verify_handoff(implicit),
                Some(&fixtures.covered),
                strict,
            );
            assert_process_exit(&output, 0);
            assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
        }
    }
}

#[test]
fn binary_cross_stage_gate_explicit_coverage_overrides_implicit_coverage() {
    let fixtures = ManifestFixtures::new();
    for strict in [false, true] {
        for (implicit, explicit, expected) in [
            (&fixtures.uncovered, &fixtures.covered, 0),
            (&fixtures.covered, &fixtures.uncovered, 2),
        ] {
            let output =
                run_manifest_gate(&linked_verify_handoff(implicit), Some(explicit), strict);
            assert_process_exit(&output, expected);
            if expected == 2 {
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(stdout.contains("uncovered_linkage"), "stdout: {stdout}");
                assert!(stdout.contains("F-002"), "stdout: {stdout}");
            }
        }
    }
}

#[test]
fn binary_cross_stage_gate_dormant_for_non_verify_with_linkage() {
    let fixtures = ManifestFixtures::new();
    for strict in [false, true] {
        for manifest in [&fixtures.missing, &fixtures.unreadable, &fixtures.malformed] {
            let input = format!(
                "PIPELINE HANDOFF — IMPLEMENTATION:\n\
                 Implementation document: /tmp/plan.md\n\
                 Manifest path: {}\n\
                 satisfies: F-002\n\
                 Status: complete\n",
                manifest.display()
            );
            for explicit in [None, Some(manifest.as_path())] {
                let output = run_manifest_gate(&input, explicit, strict);
                assert_process_exit(&output, 0);
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(stdout.contains("\"stage\": \"IMPLEMENTATION\""));
                assert!(stdout.contains("F-002"), "stdout: {stdout}");
                assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
            }
        }
    }
}

#[test]
fn binary_cross_stage_gate_dormant_for_verify_without_linkage() {
    let fixtures = ManifestFixtures::new();
    for strict in [false, true] {
        for manifest in [&fixtures.missing, &fixtures.unreadable, &fixtures.malformed] {
            let input = linked_verify_handoff(manifest).replace("satisfies: F-002\n", "");
            for explicit in [None, Some(manifest.as_path())] {
                let output = run_manifest_gate(&input, explicit, strict);
                assert_process_exit(&output, 0);
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(stdout.contains("\"stage\": \"VERIFY\""), "stdout: {stdout}");
                assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
            }
        }
    }
}

// ----- Stage-contract block gate (S7/SP2) — the ARMED gate through the process
//
// SP2 wires `scan_contract_block` into the binary: for any full-parse reply
// (pipeline handoff or `--worker-report`), a present-but-malformed
// `## Stage contract` block exits 2; an absent or well-formed block leaves the
// exit code identical to pre-SP2. These prove that end-to-end and pin the
// strict-superset property (no NEW failure for replies carrying no block).

/// A valid IMPLEMENTATION handoff whose body carries a well-formed
/// `## Stage contract` block (all four sub-sections; Inputs declares a static
/// input). The gate must PASS.
const IMPL_WITH_VALID_BLOCK: &str = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
     =============================\n\
     Implementation document: /tmp/plan.md\n\
     Manifest path: /tmp/verification.md\n\
     Status: complete\n\
     \n\
     ## Stage contract\n\
     \n\
     ### Inputs\n\
     \n\
     - Static: `crates/rsi-common/src/handoff_schema/body.rs`\n\
     \n\
     ### Process\n\
     \n\
     - Wire the gate.\n\
     \n\
     ### Outputs\n\
     \n\
     - A committed diff plus green tests.\n\
     \n\
     ### Verify\n\
     \n\
     - cargo test -p rsi-common passes.\n";

/// Same valid handoff, but the embedded block is MALFORMED — the `### Outputs`
/// sub-section is missing. The gate must FAIL (exit 2).
const IMPL_WITH_MALFORMED_BLOCK: &str = "PIPELINE HANDOFF — IMPLEMENTATION:\n\
     =============================\n\
     Implementation document: /tmp/plan.md\n\
     Manifest path: /tmp/verification.md\n\
     Status: complete\n\
     \n\
     ## Stage contract\n\
     \n\
     ### Inputs\n\
     \n\
     - Static: `crates/rsi-common/src/handoff_schema/body.rs`\n\
     \n\
     ### Process\n\
     \n\
     - Wire the gate.\n\
     \n\
     ### Verify\n\
     \n\
     - cargo test -p rsi-common passes.\n";

#[test]
fn binary_contract_block_gate_exits_2_on_malformed_block() {
    let output = run_contract(IMPL_WITH_MALFORMED_BLOCK);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a present-but-malformed stage-contract block must FAIL the armed gate\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The diagnostic names the malformed-block error and the missing sub-section.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("malformed_contract_block"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("missing_subsection:Outputs"),
        "stdout: {stdout}"
    );
}

#[test]
fn binary_contract_block_gate_exits_0_on_valid_block() {
    let output = run_contract(IMPL_WITH_VALID_BLOCK);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a well-formed stage-contract block must PASS\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn binary_contract_block_gate_dormant_when_block_absent() {
    // STRICT-SUPERSET PIN: a valid handoff that carries NO `## Stage contract`
    // block is unaffected by the new gate — exit code identical to pre-SP2 (0).
    let output = run_contract(
        "PIPELINE HANDOFF — IMPLEMENTATION:\n\
         =============================\n\
         Implementation document: /tmp/plan.md\n\
         Manifest path: /tmp/verification.md\n\
         Status: complete\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "a reply with no stage-contract block must keep its pre-SP2 exit code\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("malformed_contract_block"),
        "dormant path must emit no block diagnostic\nstdout: {stdout}"
    );
}

#[test]
fn binary_contract_block_gate_fires_on_worker_report_path() {
    // The gate is wired into the `--worker-report` path too: a WORKER REPORT
    // reply carrying a malformed block exits 2.
    let report = "WORKER REPORT:\n\
         =============================\n\
         status: complete\n\
         phase: 1\n\
         \n\
         ## Stage contract\n\
         \n\
         ### Inputs\n\
         \n\
         - The task, as understood.\n\
         \n\
         ### Process\n\
         \n\
         - Do the work.\n\
         \n\
         ### Outputs\n\
         \n\
         - A diff.\n\
         \n\
         ### Verify\n\
         \n\
         - Tests pass.\n";
    let output = run_contract_args(report, &["--worker-report"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "worker-report reply with an Inputs-declares-nothing block must FAIL\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("inputs_declare_nothing"),
        "stdout: {stdout}"
    );
}
