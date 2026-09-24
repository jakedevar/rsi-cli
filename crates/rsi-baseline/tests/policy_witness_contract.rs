#![allow(clippy::unwrap_used)]

use rsi_baseline::bounds::{
    AggregateEvidenceBytes, BOUNDS_V1, CleanupReserveFds, JsonContainerItems, JsonDepth, JsonNodes,
    LiveGroups, OrdinaryEvidenceBytes, PerCommandDeadline, PolicyItems, PreflightJsonBytes,
    ProducerFds, StringBytes, SummaryReserveBytes, TerminalReserveBytes, WholeRunDeadline,
    WitnessBytes,
};
use rsi_baseline::error::{PolicyBuildError, WitnessDecodeError};
use rsi_baseline::*;

fn sha(c: char) -> Sha256Digest {
    Sha256Digest::parse(c.to_string().repeat(64)).unwrap()
}
fn commit(c: char) -> GitCommit {
    GitCommit::parse(c.to_string().repeat(40)).unwrap()
}
fn tool(env: &[(&str, &str)], argv: &[&str]) -> ValidatedToolPolicyV1 {
    ValidatedToolPolicyV1::try_new(
        env.iter()
            .map(|(n, v)| {
                (
                    EnvironmentName::parse(*n).unwrap(),
                    EnvironmentValue::parse(*v).unwrap(),
                )
            })
            .collect(),
        argv.iter().map(|v| ArgvAtom::parse(*v).unwrap()).collect(),
    )
    .unwrap()
}
fn fixture_policy() -> ValidatedLaunchPolicyV1 {
    ValidatedLaunchPolicyV1::new(
        tool(&[("CARGO_INCREMENTAL", "0")], &["--locked"]),
        tool(&[("GIT_OPTIONAL_LOCKS", "0")], &["--no-optional-locks"]),
        tool(
            &[
                ("BASH_FD", "authenticated-bash"),
                ("MAKEFILE_FD", "fixed-makefile"),
                ("NEXTEST_JOBS", "1"),
                ("SHELL_FLAGS", "-eu"),
            ],
            &["test-suite-baseline"],
        ),
        tool(&[("NEXTEST_JOBS", "1")], &["run"]),
        tool(&[("RUST_BACKTRACE", "0")], &[]),
    )
}
fn draft(policy: ValidatedLaunchPolicyV1) -> WitnessDraftV1 {
    WitnessDraftV1 {
        accepted_implementation_commit: commit('a'),
        accepted_review_commit: commit('b'),
        accepted_verification_manifest_commit: commit('c'),
        active_ref_sha256: sha('d'),
        authorization_time: CanonicalUtcTime::parse("2026-09-02T12:34:56.000000000Z").unwrap(),
        cargo_config_sha256: sha('e'),
        failure_record_commit: GitCommit::parse("b3c15be2ebfa7bf7ed9f66288e16742e6721dc99")
            .unwrap(),
        index_sha256: sha('0'),
        make_sha256: sha('1'),
        nextest_config_sha256: sha('2'),
        output_leaf: "metrics/test-suite-baseline.json".into(),
        policy,
        producer_sha256: sha('3'),
        repository_dev: 1,
        repository_ino: 2,
        repository_path: CanonicalAbsolutePath::parse("/srv/rsi").unwrap(),
        run_nonce: CanonicalUuid::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        rust_toolchain_sha256: sha('4'),
        source_commit: commit('5'),
        source_inventory_sha256: sha('6'),
        symbolic_head: RollingRef::parse("refs/heads/rolling").unwrap(),
        tracked_stage_set_sha256: sha('7'),
    }
}
fn witness() -> ValidatedLaunchWitnessV1 {
    ValidatedLaunchWitnessV1::try_new(draft(fixture_policy())).unwrap()
}

#[test]
fn policies_are_opaque_validated_and_domain_separated() {
    let p = tool(&[("A", "v"), ("B", "w")], &["a", "b"]);
    assert!(matches!(
        ValidatedToolPolicyV1::try_new(
            vec![
                (
                    EnvironmentName::parse("B").unwrap(),
                    EnvironmentValue::parse("w").unwrap()
                ),
                (
                    EnvironmentName::parse("A").unwrap(),
                    EnvironmentValue::parse("v").unwrap()
                )
            ],
            vec![]
        ),
        Err(PolicyBuildError::Unsorted { .. })
    ));
    assert!(matches!(
        ValidatedToolPolicyV1::try_new(
            vec![
                (
                    EnvironmentName::parse("A").unwrap(),
                    EnvironmentValue::parse("v").unwrap()
                ),
                (
                    EnvironmentName::parse("A").unwrap(),
                    EnvironmentValue::parse("w").unwrap()
                )
            ],
            vec![]
        ),
        Err(PolicyBuildError::Duplicate { .. })
    ));
    assert_ne!(
        PolicyDigestV1::of(&p).as_bytes(),
        WitnessDigestV1::of(&witness()).as_bytes()
    );
    assert_eq!(
        PolicyDigestV1::of(&p).to_hex(),
        "1ac354d7bed75840d2e2581bf3641292b8bfadfe277da6a2620fba6712f7b926"
    );
}
#[test]
fn canonical_witness_rejects_hostile_nested_input() {
    let w = witness();
    let bytes = w.encode_canonical();
    assert_eq!(
        bytes,
        serde_json::to_vec(&serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()).unwrap()
    );
    assert_eq!(
        ValidatedLaunchWitnessV1::decode_canonical(&bytes).unwrap(),
        w
    );
    assert_eq!(
        WitnessDigestV1::of(&w).to_hex(),
        "10f1685f1e6bdac8a53bf72f9a1737e1158ae6adce18535009f0a5b07920f034"
    );
    for bad in [
        String::from_utf8(bytes.clone())
            .unwrap()
            .replacen('{', "{\"unknown\":true,", 1),
        String::from_utf8(bytes.clone()).unwrap().replacen(
            "\"ticket\":\"PERF-Z-BASELINE\",",
            "",
            1,
        ),
        String::from_utf8(bytes.clone()).unwrap().replacen(
            "\"cargo\":{",
            "\"cargo\":{\"additional_environment\":[],",
            1,
        ),
    ] {
        assert!(ValidatedLaunchWitnessV1::decode_canonical(bad.as_bytes()).is_err());
    }
    let duplicate = String::from_utf8(bytes).unwrap().replacen(
        "\"ticket\":\"PERF-Z-BASELINE\"",
        "\"ticket\":\"PERF-Z-BASELINE\",\"ticket\":\"PERF-Z-BASELINE\"",
        1,
    );
    assert!(matches!(
        ValidatedLaunchWitnessV1::decode_canonical(duplicate.as_bytes()),
        Err(WitnessDecodeError::Shape
            | WitnessDecodeError::Syntax
            | WitnessDecodeError::DuplicateKey)
    ));
}
#[test]
fn actual_bounds_admit_b_and_reject_b_plus_one() {
    assert!(WitnessBytes::try_new(BOUNDS_V1.witness_bytes).is_ok());
    assert!(WitnessBytes::try_new(BOUNDS_V1.witness_bytes + 1).is_err());
    assert!(StringBytes::try_new(BOUNDS_V1.string_bytes).is_ok());
    assert!(StringBytes::try_new(BOUNDS_V1.string_bytes + 1).is_err());
    assert!(
        ValidatedLaunchWitnessV1::decode_canonical(&vec![
            b' ';
            usize::try_from(
                BOUNDS_V1.witness_bytes + 1
            )
            .unwrap()
        ])
        .is_err()
    );
    assert!(JsonNodes::try_new(BOUNDS_V1.json_nodes).is_ok());
    assert!(JsonNodes::try_new(BOUNDS_V1.json_nodes + 1).is_err());
    assert!(WholeRunDeadline::try_new(BOUNDS_V1.whole_run_deadline_seconds).is_ok());
    assert!(WholeRunDeadline::try_new(BOUNDS_V1.whole_run_deadline_seconds + 1).is_err());
}

fn canonical() -> String {
    String::from_utf8(witness().encode_canonical()).unwrap()
}
fn decode_error(input: impl AsRef<str>) -> WitnessDecodeError {
    ValidatedLaunchWitnessV1::decode_canonical(input.as_ref().as_bytes()).unwrap_err()
}
#[allow(clippy::needless_pass_by_value)]
fn assert_decode_error(row: &str, input: &[u8], expected: WitnessDecodeError) {
    let actual = ValidatedLaunchWitnessV1::decode_canonical(input).unwrap_err();
    assert_eq!(actual, expected, "{row}");
}
fn policy_at_count(count: usize) -> Vec<(EnvironmentName, EnvironmentValue)> {
    (0..count)
        .map(|index| {
            (
                EnvironmentName::parse(format!("Z{index:06}")).unwrap(),
                EnvironmentValue::parse("v").unwrap(),
            )
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn numeric_taxonomy_is_feature_independent() {
    let base = canonical();
    let with_dev = |number: &str| {
        base.replacen(
            "\"repository_dev\":1",
            &format!("\"repository_dev\":{number}"),
            1,
        )
    };
    let with_schema = |number: &str| {
        base.replacen(
            "\"schema_version\":1",
            &format!("\"schema_version\":{number}"),
            1,
        )
    };
    let truncated_after_dev = |number: &str| {
        let input = with_dev(number);
        let needle = format!("\"repository_dev\":{number}");
        let end = input.find(&needle).unwrap() + needle.len();
        input[..end].to_owned()
    };
    let duplicate_ticket_before_dev = |input: &str| {
        input.replacen(
            '{',
            "{\"ticket\":\"PERF-Z-BASELINE\",\"ticket\":\"PERF-Z-BASELINE\",",
            1,
        )
    };

    for (row, number, repository_dev) in [
        ("NUM-I-001", "1", 1),
        ("NUM-I-002", "42", 42),
        ("NUM-I-003", "18446744073709551615", u64::MAX),
    ] {
        let input = with_dev(number);
        let decoded = ValidatedLaunchWitnessV1::decode_canonical(input.as_bytes())
            .unwrap_or_else(|error| panic!("{row}: {error:?}"));
        assert_eq!(decoded.repository_dev(), repository_dev, "{row}");
        assert_eq!(decoded.repository_ino(), 2, "{row}");
        assert_eq!(decoded.schema_version(), 1, "{row}");
        assert_eq!(decoded.encode_canonical(), input.as_bytes(), "{row}");
    }

    for (row, input, expected) in [
        ("NUM-I-004", with_dev("0"), WitnessDecodeError::InvalidValue),
        ("NUM-I-005", with_dev("-0"), WitnessDecodeError::Shape),
        ("NUM-I-006", with_dev("-1"), WitnessDecodeError::Shape),
        (
            "NUM-I-007",
            with_dev("-9223372036854775808"),
            WitnessDecodeError::Shape,
        ),
        (
            "NUM-I-008",
            with_dev("-9223372036854775809"),
            WitnessDecodeError::Shape,
        ),
        (
            "NUM-I-009",
            with_dev("18446744073709551616"),
            WitnessDecodeError::Shape,
        ),
        (
            "NUM-I-010",
            with_dev("9999999999999999999999999999999999999999999999999999999999999999"),
            WitnessDecodeError::Shape,
        ),
        (
            "NUM-I-011",
            with_schema("4294967295"),
            WitnessDecodeError::UnsupportedWitnessVersion {
                found: 4_294_967_295,
            },
        ),
        (
            "NUM-I-012",
            with_schema("4294967296"),
            WitnessDecodeError::Shape,
        ),
    ] {
        assert_decode_error(row, input.as_bytes(), expected);
    }

    for (row, number) in [
        ("NUM-N-001", "0.0"),
        ("NUM-N-002", "1.0"),
        ("NUM-N-003", "-0.1"),
        ("NUM-N-004", "-1.0"),
        ("NUM-N-005", "10.25"),
        ("NUM-N-006", "1e0"),
        ("NUM-N-007", "1E+0"),
        ("NUM-N-008", "1e-1"),
        ("NUM-N-009", "0e0"),
        ("NUM-N-010", "-1E+9"),
        ("NUM-N-011", "10.25e+3"),
        ("NUM-N-012", "1e400"),
        ("NUM-N-013", "0e999999999999999999999999999999999999"),
        ("NUM-N-014", "-18446744073709551616.0"),
    ] {
        let input = with_dev(number);
        assert_decode_error(row, input.as_bytes(), WitnessDecodeError::NonInteger);
    }

    for (row, number) in [
        ("NUM-S-001", ""),
        ("NUM-S-002", "-"),
        ("NUM-S-003", ".1"),
        ("NUM-S-004", "-.1"),
        ("NUM-S-005", "+1"),
        ("NUM-S-006", "--1"),
        ("NUM-S-007", "-+1"),
        ("NUM-S-008", "00"),
        ("NUM-S-009", "01"),
        ("NUM-S-010", "-01"),
        ("NUM-S-011", "1."),
        ("NUM-S-012", "1.e0"),
        ("NUM-S-013", "1..0"),
        ("NUM-S-014", "1e"),
        ("NUM-S-015", "1E"),
        ("NUM-S-016", "1e+"),
        ("NUM-S-017", "1e-"),
        ("NUM-S-018", "1e+-1"),
        ("NUM-S-019", "1e--1"),
        ("NUM-S-020", "1ee1"),
        ("NUM-S-021", "1e1.0"),
        ("NUM-S-022", "1x"),
        ("NUM-S-023", "1_0"),
        ("NUM-S-024", "0x1"),
        ("NUM-S-025", "NaN"),
        ("NUM-S-026", "Infinity"),
        ("NUM-S-027", "-Infinity"),
    ] {
        let input = with_dev(number);
        assert_decode_error(row, input.as_bytes(), WitnessDecodeError::Syntax);
    }

    for (row, input, expected) in [
        (
            "NUM-D-001",
            with_dev("1 "),
            WitnessDecodeError::NonCanonical,
        ),
        (
            "NUM-D-002",
            with_dev("1\n"),
            WitnessDecodeError::NonCanonical,
        ),
        (
            "NUM-D-003",
            with_dev("1.0 "),
            WitnessDecodeError::NonInteger,
        ),
        (
            "NUM-D-004",
            with_dev("1e0\n"),
            WitnessDecodeError::NonInteger,
        ),
        ("NUM-D-005", with_dev("1 2"), WitnessDecodeError::Syntax),
        ("NUM-D-006", with_dev("1,"), WitnessDecodeError::Syntax),
        ("NUM-D-007", with_dev("1]"), WitnessDecodeError::Syntax),
        ("NUM-D-008", with_dev("1}"), WitnessDecodeError::Syntax),
        ("NUM-D-009", with_dev("1:"), WitnessDecodeError::Syntax),
        ("NUM-D-010", with_dev("1\"x\""), WitnessDecodeError::Syntax),
        ("NUM-D-011", with_dev("1[2]"), WitnessDecodeError::Syntax),
        ("NUM-D-012", with_dev("1+2"), WitnessDecodeError::Syntax),
        ("NUM-D-013", with_dev("1-2"), WitnessDecodeError::Syntax),
        ("NUM-D-014", with_dev("1.0x"), WitnessDecodeError::Syntax),
        ("NUM-D-015", with_dev("1e0x"), WitnessDecodeError::Syntax),
        (
            "NUM-D-016",
            truncated_after_dev("1"),
            WitnessDecodeError::Syntax,
        ),
        (
            "NUM-D-017",
            truncated_after_dev("1.0"),
            WitnessDecodeError::Syntax,
        ),
        (
            "NUM-D-018",
            truncated_after_dev("1e"),
            WitnessDecodeError::Syntax,
        ),
        (
            "NUM-D-019",
            truncated_after_dev("1e+"),
            WitnessDecodeError::Syntax,
        ),
        ("NUM-D-020", "1".to_owned(), WitnessDecodeError::Shape),
        ("NUM-D-021", "1 ".to_owned(), WitnessDecodeError::Shape),
        (
            "NUM-D-022",
            "1.0".to_owned(),
            WitnessDecodeError::NonInteger,
        ),
        ("NUM-D-023", "1e".to_owned(), WitnessDecodeError::Syntax),
        ("NUM-D-024", "1]".to_owned(), WitnessDecodeError::Syntax),
        ("NUM-D-025", "[1]".to_owned(), WitnessDecodeError::Shape),
        ("NUM-D-026", "[1,2]".to_owned(), WitnessDecodeError::Shape),
        ("NUM-D-027", "[1,]".to_owned(), WitnessDecodeError::Syntax),
        (
            "NUM-D-028",
            "{\"x\":1}".to_owned(),
            WitnessDecodeError::Shape,
        ),
        (
            "NUM-D-029",
            "{\"x\":1,}".to_owned(),
            WitnessDecodeError::Syntax,
        ),
    ] {
        assert_decode_error(row, input.as_bytes(), expected);
    }

    let mut missing_final_brace = with_dev("1.0");
    assert_eq!(missing_final_brace.pop(), Some('}'));
    assert_decode_error(
        "NUM-D-030",
        missing_final_brace.as_bytes(),
        WitnessDecodeError::Syntax,
    );

    let duplicate_then_malformed = duplicate_ticket_before_dev(&with_dev("1e"));
    assert_decode_error(
        "NUM-D-031",
        duplicate_then_malformed.as_bytes(),
        WitnessDecodeError::Syntax,
    );

    let duplicate_then_noninteger = duplicate_ticket_before_dev(&with_dev("1e0"));
    assert_decode_error(
        "NUM-D-032",
        duplicate_then_noninteger.as_bytes(),
        WitnessDecodeError::DuplicateKey,
    );

    let attacker_duplicate = base.replacen('{', "{\"evil\\nkey\":0,\"evil\\nkey\":0,", 1);
    let attacker_error =
        ValidatedLaunchWitnessV1::decode_canonical(attacker_duplicate.as_bytes()).unwrap_err();
    assert_eq!(
        attacker_error,
        WitnessDecodeError::DuplicateKey,
        "NUM-D-033"
    );
    for diagnostic in [format!("{attacker_error:?}"), format!("{attacker_error}")] {
        assert!(!diagnostic.contains("evil"), "NUM-D-033: {diagnostic:?}");
        assert!(!diagnostic.contains("line"), "NUM-D-033: {diagnostic:?}");
        assert!(!diagnostic.contains("column"), "NUM-D-033: {diagnostic:?}");
        assert!(!diagnostic.contains('\n'), "NUM-D-033: {diagnostic:?}");
    }

    let mut invalid_utf8 = with_dev("1").into_bytes();
    let dev_token = b"\"repository_dev\":1";
    let dev_offset = invalid_utf8
        .windows(dev_token.len())
        .position(|window| window == dev_token)
        .unwrap();
    invalid_utf8[dev_offset + dev_token.len() - 1] = 0xff;
    assert_decode_error("NUM-D-034", &invalid_utf8, WitnessDecodeError::Utf8);

    let trailing_root = format!("{base}1");
    assert_decode_error(
        "NUM-D-035",
        trailing_root.as_bytes(),
        WitnessDecodeError::Syntax,
    );
}

#[test]
fn witness_getters_expose_every_reconciliation_fact() {
    let w = witness();
    assert_eq!(
        w.accepted_implementation_commit().as_str(),
        commit('a').as_str()
    );
    assert_eq!(w.accepted_review_commit().as_str(), commit('b').as_str());
    assert_eq!(
        w.accepted_verification_manifest_commit().as_str(),
        commit('c').as_str()
    );
    assert_eq!(w.active_ref_sha256().as_str(), sha('d').as_str());
    assert_eq!(
        w.authorization_time().as_str(),
        "2026-09-02T12:34:56.000000000Z"
    );
    assert_eq!(w.cargo_config_sha256().as_str(), sha('e').as_str());
    assert_eq!(
        w.failure_record_commit().as_str(),
        "b3c15be2ebfa7bf7ed9f66288e16742e6721dc99"
    );
    assert_eq!(w.index_sha256().as_str(), sha('0').as_str());
    assert_eq!(w.make_sha256().as_str(), sha('1').as_str());
    assert_eq!(w.nextest_config_sha256().as_str(), sha('2').as_str());
    assert_eq!(w.producer_sha256().as_str(), sha('3').as_str());
    assert_eq!(w.rust_toolchain_sha256().as_str(), sha('4').as_str());
    assert_eq!(w.source_commit().as_str(), commit('5').as_str());
    assert_eq!(w.source_inventory_sha256().as_str(), sha('6').as_str());
    assert_eq!(w.tracked_stage_set_sha256().as_str(), sha('7').as_str());
    assert_eq!(w.repository_dev(), 1);
    assert_eq!(w.repository_ino(), 2);
    assert_eq!(w.repository_path().as_str(), "/srv/rsi");
    assert_eq!(w.symbolic_head().as_str(), "refs/heads/rolling");
    assert_eq!(
        w.run_nonce().as_str(),
        "550e8400-e29b-41d4-a716-446655440000"
    );
    assert_eq!(w.output_leaf(), "metrics/test-suite-baseline.json");
    assert!(w.output_absent_at_launch());
    assert!(w.single_invocation());
    assert_eq!(w.schema_version(), 1);
    assert_eq!(w.ticket(), "PERF-Z-BASELINE");
    assert_eq!(
        w.cargo_policy_sha256(),
        PolicyDigestV1::of(w.policy().cargo())
    );
    assert_eq!(w.git_policy_sha256(), PolicyDigestV1::of(w.policy().git()));
    assert_eq!(
        w.make_policy_sha256(),
        PolicyDigestV1::of(w.policy().make())
    );
    assert_eq!(
        w.nextest_policy_sha256(),
        PolicyDigestV1::of(w.policy().nextest())
    );
    assert_eq!(
        w.rust_policy_sha256(),
        PolicyDigestV1::of(w.policy().rust())
    );
}

#[test]
fn canonical_decode_rejects_shape_attacks_at_every_nested_depth() {
    let input = canonical();
    for bad in [
        input.replacen('{', "{\"unknown\":true,", 1),
        input.replacen("\"policy\":{", "\"policy\":{\"unknown\":true,", 1),
        input.replacen("\"cargo\":{", "\"cargo\":{\"unknown\":true,", 1),
        input.replacen(
            "{\"name\":\"CARGO_INCREMENTAL\",\"value\":\"0\"}",
            "{\"name\":\"CARGO_INCREMENTAL\",\"value\":\"0\",\"unknown\":true}",
            1,
        ),
    ] {
        assert!(matches!(decode_error(bad), WitnessDecodeError::Shape));
    }
    let mut value: serde_json::Value = serde_json::from_str(&input).unwrap();
    value.as_object_mut().unwrap().remove("ticket");
    assert!(matches!(
        decode_error(serde_json::to_string(&value).unwrap()),
        WitnessDecodeError::Shape
    ));
    let mut value: serde_json::Value = serde_json::from_str(&input).unwrap();
    value["policy"].as_object_mut().unwrap().remove("git");
    assert!(matches!(
        decode_error(serde_json::to_string(&value).unwrap()),
        WitnessDecodeError::Shape
    ));
    let mut value: serde_json::Value = serde_json::from_str(&input).unwrap();
    value["policy"]["cargo"]
        .as_object_mut()
        .unwrap()
        .remove("allowed_argv_suffix");
    assert!(matches!(
        decode_error(serde_json::to_string(&value).unwrap()),
        WitnessDecodeError::Shape
    ));
    let mut value: serde_json::Value = serde_json::from_str(&input).unwrap();
    value["policy"]["cargo"]["additional_environment"][0]
        .as_object_mut()
        .unwrap()
        .remove("name");
    assert!(matches!(
        decode_error(serde_json::to_string(&value).unwrap()),
        WitnessDecodeError::Shape
    ));
    for bad in [
        input.replacen(
            "\"ticket\":\"PERF-Z-BASELINE\"",
            "\"ticket\":\"PERF-Z-BASELINE\",\"ticket\":\"PERF-Z-BASELINE\"",
            1,
        ),
        input.replacen("\"cargo\":{", "\"cargo\":{},\"cargo\":{", 1),
        input.replacen(
            "\"allowed_argv_suffix\":[\"--locked\"]",
            "\"allowed_argv_suffix\":[],\"allowed_argv_suffix\":[\"--locked\"]",
            1,
        ),
        input.replacen(
            "{\"name\":\"CARGO_INCREMENTAL\",\"value\":\"0\"}",
            "{\"name\":\"CARGO_INCREMENTAL\",\"name\":\"CARGO_INCREMENTAL\",\"value\":\"0\"}",
            1,
        ),
    ] {
        assert!(matches!(
            decode_error(bad),
            WitnessDecodeError::DuplicateKey
        ));
    }
}

#[test]
fn canonical_decode_rejects_order_whitespace_and_policy_sequence_attacks() {
    let input = canonical();
    assert!(matches!(
        decode_error(format!(" {input}")),
        WitnessDecodeError::NonCanonical
    ));
    let reordered = input
        .replacen(",\"ticket\":\"PERF-Z-BASELINE\"", "", 1)
        .replacen('{', "{\"ticket\":\"PERF-Z-BASELINE\",", 1);
    assert!(matches!(
        decode_error(reordered),
        WitnessDecodeError::NonCanonical
    ));
    let unsorted_env = input.replacen("{\"name\":\"BASH_FD\",\"value\":\"authenticated-bash\"},{\"name\":\"MAKEFILE_FD\",\"value\":\"fixed-makefile\"}", "{\"name\":\"MAKEFILE_FD\",\"value\":\"fixed-makefile\"},{\"name\":\"BASH_FD\",\"value\":\"authenticated-bash\"}", 1);
    assert!(matches!(
        decode_error(unsorted_env),
        WitnessDecodeError::InvalidValue
    ));
    let duplicate_env = input.replacen("\"MAKEFILE_FD\"", "\"BASH_FD\"", 1);
    assert!(matches!(
        decode_error(duplicate_env),
        WitnessDecodeError::InvalidValue
    ));
    let unsorted_argv = input.replacen(
        "\"allowed_argv_suffix\":[\"--locked\"]",
        "\"allowed_argv_suffix\":[\"--z\",\"--a\"]",
        1,
    );
    assert!(matches!(
        decode_error(unsorted_argv),
        WitnessDecodeError::InvalidValue
    ));
    let duplicate_argv = input.replacen(
        "\"allowed_argv_suffix\":[\"--locked\"]",
        "\"allowed_argv_suffix\":[\"--locked\",\"--locked\"]",
        1,
    );
    assert!(matches!(
        decode_error(duplicate_argv),
        WitnessDecodeError::InvalidValue
    ));
    assert!(matches!(
        ValidatedToolPolicyV1::try_new(
            vec![],
            vec![ArgvAtom::parse("z").unwrap(), ArgvAtom::parse("a").unwrap()]
        ),
        Err(PolicyBuildError::Unsorted { .. })
    ));
    assert!(matches!(
        ValidatedToolPolicyV1::try_new(
            vec![],
            vec![ArgvAtom::parse("a").unwrap(), ArgvAtom::parse("a").unwrap()]
        ),
        Err(PolicyBuildError::Duplicate { .. })
    ));
}

#[test]
fn scalar_and_witness_fixed_facts_reject_invalid_values() {
    assert!(EnvironmentName::parse("HOME").is_err());
    assert!(EnvironmentName::parse("lower").is_err());
    assert!(EnvironmentValue::parse("").is_err());
    assert!(EnvironmentValue::parse("x\0y").is_err());
    assert!(ArgvAtom::parse("").is_err());
    assert!(ArgvAtom::parse("x\0y").is_err());
    assert!(CanonicalAbsolutePath::parse("relative").is_err());
    assert!(Sha256Digest::parse("A".repeat(64)).is_err());
    assert!(GitCommit::parse("x".repeat(40)).is_err());
    assert!(CanonicalUuid::parse("550E8400-E29B-41D4-A716-446655440000").is_err());
    assert!(RollingRef::parse("refs/heads/main").is_err());
    assert!(CanonicalUtcTime::parse("2026-09-02T12:34:56Z").is_err());
    assert!(
        EnvironmentValue::parse("v".repeat(usize::try_from(BOUNDS_V1.string_bytes).unwrap()))
            .is_ok()
    );
    assert!(
        EnvironmentValue::parse("v".repeat(usize::try_from(BOUNDS_V1.string_bytes + 1).unwrap()))
            .is_err()
    );
    assert!(ArgvAtom::parse("a".repeat(usize::try_from(BOUNDS_V1.string_bytes).unwrap())).is_ok());
    assert!(
        ArgvAtom::parse("a".repeat(usize::try_from(BOUNDS_V1.string_bytes + 1).unwrap())).is_err()
    );
    let input = canonical();
    for bad in [
        input.replacen("\"repository_dev\":1", "\"repository_dev\":0", 1),
        input.replacen("\"repository_ino\":2", "\"repository_ino\":0", 1),
        input.replacen(
            "\"failure_record_commit\":\"b3c15be2ebfa7bf7ed9f66288e16742e6721dc99\"",
            "\"failure_record_commit\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            1,
        ),
        input.replacen(
            "\"output_leaf\":\"metrics/test-suite-baseline.json\"",
            "\"output_leaf\":\"metrics/other.json\"",
            1,
        ),
        input.replacen("\"ticket\":\"PERF-Z-BASELINE\"", "\"ticket\":\"OTHER\"", 1),
        input.replacen(
            "\"output_absent_at_launch\":true",
            "\"output_absent_at_launch\":false",
            1,
        ),
        input.replacen(
            "\"single_invocation\":true",
            "\"single_invocation\":false",
            1,
        ),
        input.replacen("\"schema_version\":1", "\"schema_version\":2", 1),
    ] {
        assert!(ValidatedLaunchWitnessV1::decode_canonical(bad.as_bytes()).is_err());
    }
    assert!(matches!(
        decode_error(input.replacen("\"repository_dev\":1", "\"repository_dev\":1.0", 1)),
        WitnessDecodeError::NonInteger
    ));
}

#[test]
fn oversized_programmatic_draft_refuses_at_the_witness_boundary() {
    let environment = (0..2_000)
        .map(|index| {
            (
                EnvironmentName::parse(format!("Z{index:06}")).unwrap(),
                EnvironmentValue::parse("v".repeat(64)).unwrap(),
            )
        })
        .collect();
    let oversized = ValidatedLaunchPolicyV1::new(
        ValidatedToolPolicyV1::try_new(environment, vec![]).unwrap(),
        fixture_policy().git().clone(),
        fixture_policy().make().clone(),
        fixture_policy().nextest().clone(),
        fixture_policy().rust().clone(),
    );
    assert!(matches!(
        ValidatedLaunchWitnessV1::try_new(draft(oversized)),
        Err(rsi_baseline::error::WitnessBuildError::TooLarge)
    ));
}

#[test]
fn digests_are_exact_and_all_policy_binding_mutations_reject() {
    let w = witness();
    let input = canonical();
    assert_eq!(
        [
            w.cargo_policy_sha256().to_hex(),
            w.git_policy_sha256().to_hex(),
            w.make_policy_sha256().to_hex(),
            w.nextest_policy_sha256().to_hex(),
            w.rust_policy_sha256().to_hex(),
        ],
        [
            "e439a13cec15a4927e383cead1de09661bdfe8b8901cc4a8b39f9e4da67b322b",
            "b8a5d50805f337b8b00f414669a03c7218d034dc287defc4589c1fe894398038",
            "2b2c89f5a776d2373da2e10c2af1260f410797357918bef705148af0f16287d4",
            "dd933a1290260a806e34439847d5788e0aa956cafdae2cd612c47fa767ede587",
            "2ec02dbdcca7091ede3933bd7683fbe86a9437ea1d368a74cee68a896a94d7d0",
        ]
    );
    assert_eq!(
        PolicyDigestV1::of(w.policy().cargo()).to_hex(),
        "e439a13cec15a4927e383cead1de09661bdfe8b8901cc4a8b39f9e4da67b322b"
    );
    assert_eq!(
        WitnessDigestV1::of(&w).to_hex(),
        "10f1685f1e6bdac8a53bf72f9a1737e1158ae6adce18535009f0a5b07920f034"
    );
    for field in [
        "cargo_policy_sha256",
        "git_policy_sha256",
        "make_policy_sha256",
        "nextest_policy_sha256",
        "rust_policy_sha256",
    ] {
        let needle = format!("\"{field}\":\"");
        let changed = input.replacen(&needle, &format!("{needle}0"), 1);
        assert!(matches!(
            decode_error(changed),
            WitnessDecodeError::DigestMismatch
        ));
    }
    let changed = input.replacen(
        "\"source_commit\":\"5555555555555555555555555555555555555555\"",
        "\"source_commit\":\"6555555555555555555555555555555555555555\"",
        1,
    );
    let changed = ValidatedLaunchWitnessV1::decode_canonical(changed.as_bytes()).unwrap();
    assert_ne!(WitnessDigestV1::of(&w), WitnessDigestV1::of(&changed));
}

#[test]
fn every_profile_bound_and_real_policy_count_admit_b_then_reject_b_plus_one() {
    macro_rules! boundary {
        ($type:ty, $value:expr) => {{
            assert!(<$type>::try_new($value).is_ok());
            assert!(<$type>::try_new($value + 1).is_err());
        }};
    }
    boundary!(WitnessBytes, BOUNDS_V1.witness_bytes);
    boundary!(StringBytes, BOUNDS_V1.string_bytes);
    boundary!(PolicyItems, BOUNDS_V1.json_container_items);
    boundary!(JsonDepth, BOUNDS_V1.json_depth);
    boundary!(JsonNodes, BOUNDS_V1.json_nodes);
    boundary!(JsonContainerItems, BOUNDS_V1.json_container_items);
    boundary!(PreflightJsonBytes, BOUNDS_V1.preflight_json_bytes);
    boundary!(AggregateEvidenceBytes, BOUNDS_V1.aggregate_evidence_bytes);
    boundary!(TerminalReserveBytes, BOUNDS_V1.terminal_reserve_bytes);
    boundary!(SummaryReserveBytes, BOUNDS_V1.summary_reserve_bytes);
    boundary!(OrdinaryEvidenceBytes, BOUNDS_V1.ordinary_evidence_bytes);
    boundary!(ProducerFds, BOUNDS_V1.producer_fds);
    boundary!(CleanupReserveFds, BOUNDS_V1.reserved_cleanup_fds);
    boundary!(LiveGroups, BOUNDS_V1.live_groups);
    boundary!(PerCommandDeadline, BOUNDS_V1.per_command_deadline_seconds);
    boundary!(WholeRunDeadline, BOUNDS_V1.whole_run_deadline_seconds);
    assert_eq!(
        BOUNDS_V1.ordinary_evidence_bytes
            + BOUNDS_V1.terminal_reserve_bytes
            + BOUNDS_V1.summary_reserve_bytes,
        BOUNDS_V1.aggregate_evidence_bytes
    );
    assert_eq!(BOUNDS_V1.run_root_mode, 0o700);
    assert_eq!(BOUNDS_V1.regular_file_mode, 0o600);
    assert_eq!(BOUNDS_V1.live_groups, 1);
    assert!(WitnessBytes::try_new(0).unwrap().checked_sub(1).is_err());
    assert!(
        WitnessBytes::try_new(WitnessBytes::MAXIMUM)
            .unwrap()
            .checked_add(1)
            .is_err()
    );
    assert!(
        ValidatedToolPolicyV1::try_new(
            policy_at_count(usize::try_from(BOUNDS_V1.json_container_items).unwrap()),
            vec![]
        )
        .is_ok()
    );
    assert!(matches!(
        ValidatedToolPolicyV1::try_new(
            policy_at_count(usize::try_from(BOUNDS_V1.json_container_items + 1).unwrap()),
            vec![]
        ),
        Err(PolicyBuildError::TooMany { .. })
    ));
    let argv_at_count = |count| {
        (0..count)
            .map(|index| ArgvAtom::parse(format!("a{index:06}")).unwrap())
            .collect::<Vec<_>>()
    };
    assert!(
        ValidatedToolPolicyV1::try_new(
            vec![],
            argv_at_count(usize::try_from(BOUNDS_V1.json_container_items).unwrap())
        )
        .is_ok()
    );
    assert!(matches!(
        ValidatedToolPolicyV1::try_new(
            vec![],
            argv_at_count(usize::try_from(BOUNDS_V1.json_container_items + 1).unwrap())
        ),
        Err(PolicyBuildError::TooMany { .. })
    ));
    let exact = vec![b' '; usize::try_from(BOUNDS_V1.witness_bytes).unwrap()];
    assert!(!matches!(
        ValidatedLaunchWitnessV1::decode_canonical(&exact),
        Err(WitnessDecodeError::TooLarge)
    ));
    assert!(matches!(
        ValidatedLaunchWitnessV1::decode_canonical(&vec![
            b' ';
            usize::try_from(
                BOUNDS_V1.witness_bytes + 1
            )
            .unwrap()
        ]),
        Err(WitnessDecodeError::TooLarge)
    ));
}
