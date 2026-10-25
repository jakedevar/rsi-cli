//! Hermetic ports of test_cloud_sweep_artifacts: injected fingerprint probes,
//! literal v1 wire bytes, and real report -> verdict execution without Python.
mod common;
use rsi_cloud::sweep::{
    Classification, JsonSource, build_verdict, json_report, pretty_json, report,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const START: &str = "2026-09-27T00:00:00Z";
const END: &str = "2026-09-27T00:01:00Z";
const FAIL_LOG: &str = "FAIL [ 00:01 ] (1/2) test a::known\nFAIL [ 00:01 ] (2/2) test b::other\n";
const CRASHED: &str =
    "session::h2_rotation_successor_interleaving_matrix_preserves_one_authority_projection";
const CRASH_LOG: &str = concat!(
    "        PASS [   2.022s] ( 1/409) rsid session::tests::passed\n",
    "     SIGABRT [   0.512s] (177/409) rsid session::h2_rotation_successor_interleaving_matrix_preserves_one_authority_projection\n",
    "     TIMEOUT [ 244.093s] (178/409) rsid session::tests::timed_out\n",
    "     Summary [ 300.000s] 409 tests run: 407 passed, 1 timed out, 0 skipped\nerror: test run failed\n"
);

struct Source;
fn shards() -> Vec<String> {
    [("store", 4), ("session", 5), ("memory", 2), ("other", 5)]
        .into_iter()
        .flat_map(|(group, count)| (1..=count).map(move |n| format!("{group}-{n:02}")))
        .collect()
}
impl JsonSource for Source {
    fn command(&self, program: &str, args: &[&str]) -> Result<String, String> {
        Ok(match (program, args) {
            ("git", ["rev-parse", "HEAD"]) => SHA.into(),
            ("git", ["rev-parse", tree]) if *tree == format!("{SHA}^{{tree}}") => "b".repeat(40),
            ("git", ["rev-parse", blob]) if blob.starts_with("HEAD:") => "c".repeat(40),
            ("rustc", ["-Vv"]) => "rustc 1.94\nhost: x86_64-unknown-linux-gnu".into(),
            ("cargo", ["-V"]) => "cargo 1.94".into(),
            ("cargo", ["nextest", "--version"]) => "nextest 0.9".into(),
            _ => panic!("unexpected probe {program} {args:?}"),
        })
    }
    fn hostname(&self) -> Result<String, String> {
        Ok("fixture-host".into())
    }
    fn host_class(&self) -> Result<String, String> {
        Ok("linux-x86_64@fixture-host:0123456789abcdef".into())
    }
    fn shards(&self) -> Result<Vec<String>, String> {
        Ok(shards())
    }
}
fn write_lane(root: &Path, label: &str, code: i32, log: &str) {
    for sub in ["status", "logs"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(
        root.join(format!("status/{label}.tsv")),
        format!("{label}\t{code}\n"),
    )
    .unwrap();
    fs::write(root.join(format!("logs/{label}.log")), log).unwrap();
}
fn artifact(root: &Path) -> Value {
    json_report(SHA, START, END, root, "nextest", &Source).unwrap()
}
fn human(root: &Path) -> String {
    report(SHA, START, END, root, &root.join("no-seed")).unwrap() + "\n"
}
fn collected(root: &Path, lanes: u32, names: &[&str]) {
    fs::create_dir_all(root.join("failure-logs")).unwrap();
    let names = if names.is_empty() {
        "- none\n".into()
    } else {
        names
            .iter()
            .map(|n| format!("- `{n}`\n"))
            .collect::<String>()
    };
    fs::write(root.join("QA.md"), format!("# Cloud QA sweep\n\nTip SHA: `{SHA}`  \nLanes: {lanes}  \n\nSeed set: absent; classification unavailable\nUnclassified failing names (seed names alone do not verify failure signatures):\n{names}FLAKE lines:\n- none observed\n")).unwrap();
}
fn gzip(path: &Path, log: &str) {
    let mut file = flate2::write::GzEncoder::new(
        fs::File::create(path).unwrap(),
        flate2::Compression::default(),
    );
    file.write_all(log.as_bytes()).unwrap();
    file.finish().unwrap();
}
fn classifier(root: &Path, known: &[&str]) -> PathBuf {
    let snapshot = root.join("snapshot.json");
    fs::write(&snapshot, known.join("\n")).unwrap();
    snapshot
}
fn run_verdict(root: &Path, snapshot: &Path, present: bool) -> String {
    let before = fs::read_to_string(root.join("QA.md")).unwrap();
    let bin = if present {
        PathBuf::from(env!("CARGO_BIN_EXE_rsi-cloud-sweep-fake"))
    } else {
        root.join("missing-classifier")
    };
    let output = common::rsi_cloud()
        .arg("sweep-verdict")
        .arg(root)
        .arg("--snapshot")
        .arg(snapshot)
        .arg("--classifier")
        .arg(bin)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let section = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("QA.md")).unwrap(),
        before + &section
    );
    section
}

#[test]
fn json_covers_every_lane_and_exact_fingerprint_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for shard in shards() {
        write_lane(
            root,
            &format!("rsid-{shard}"),
            0,
            "Summary [ 00:01 ] 1 tests run: 1 passed, 0 failed, 0 skipped\n",
        );
    }
    for label in [
        "rsid-integrations",
        "rsid-bins",
        "rsid-doctests",
        "rsi",
        "rsi-common",
        "other-workspace",
        "other-doctests",
    ] {
        write_lane(root, label, 0, "");
    }
    write_lane(
        root,
        "other-workspace",
        100,
        "FAIL [ 00:01 ] (1/1) test other::fails\n",
    );
    let report = artifact(root);
    assert_eq!(report["complete"], true);
    assert_eq!(
        report["shards"][0]["environment"],
        json!({"spec_env": {"CARGO_BUILD_JOBS": "4"}, "tmpdir_class": "disk"})
    );
    let inputs = json!({"rustc": "rustc 1.94\nhost: x86_64-unknown-linux-gnu", "cargo": "cargo 1.94", "nextest": "nextest 0.9", "target_triple": "x86_64-unknown-linux-gnu", "host_class": "linux-x86_64@fixture-host:0123456789abcdef", "feature": "test-shard-store-01", "jobs": 4, "test_threads": 4, "runner_blob": "c".repeat(40), "checker_blob": "c".repeat(40), "nextest_config_blob": "c".repeat(40)});
    assert_eq!(report["shards"][0]["fingerprint"]["inputs"], inputs);
    assert_eq!(
        report["shards"][0]["fingerprint"]["digest"],
        "fcebd5cbe0787dced98c3039249a75a9565fad17d5239bb99832c00036f85263"
    );
    assert_eq!(
        report["extra_lanes"],
        json!([{"name": "other-workspace", "exit_code": 100, "failing_tests": ["other::fails"]}, {"name": "other-doctests", "exit_code": 0, "failing_tests": []}])
    );
    fs::remove_file(root.join("status/other-workspace.tsv")).unwrap();
    assert_eq!(artifact(root)["complete"], false);
}

#[test]
fn partial_json_is_byte_identical_and_legacy_cargo_test_stays_partial() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lane(
        root,
        "rolling-library-qa",
        100,
        "FAIL [ 00:01 ] (1/1) rsid store::fails\n",
    );
    let expected = include_str!("fixtures/sweep-partial.json.in").replace(
        "$RESULT",
        &rsi_cloud::sweep::legacy_json(&json!(root.join("report.json").to_string_lossy()), true),
    );
    assert_eq!(pretty_json(&artifact(root)) + "\n", expected);
    let legacy = json_report(SHA, START, END, root, "cargo-test", &Source).unwrap();
    let expected = include_str!("fixtures/sweep-cargo-test.json.in").replace(
        "$RESULT",
        &rsi_cloud::sweep::legacy_json(&json!(root.join("report.json").to_string_lossy()), true),
    );
    assert_eq!(pretty_json(&legacy) + "\n", expected);
    assert_eq!(
        json_report(SHA, START, END, root, "wrong", &Source).unwrap_err(),
        "runner must be nextest or cargo-test"
    );
    assert_eq!(
        json_report("wrong", START, END, root, "nextest", &Source).unwrap_err(),
        "report source HEAD differs from swept SHA"
    );
    assert_eq!(
        pretty_json(&json!({"z": "☁😀", "a": [{"b": []}]})),
        "{\n  \"a\": [\n    {\n      \"b\": []\n    }\n  ],\n  \"z\": \"\\u2601\\ud83d\\ude00\"\n}"
    );
}

#[test]
fn absent_seed_report_bytes_and_seeded_review_count() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lane(
        root,
        "other-workspace",
        100,
        "FAIL [ 00:01 ] (1/1) test other::fails\n",
    );
    let expected = include_str!("fixtures/sweep-report.md.in").replace("$BR", "  ");
    assert_eq!(human(root), expected);
    let output = common::rsi_cloud()
        .args(["sweep-report", SHA, START, END])
        .arg(root)
        .arg(root.join("no-seed"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, expected.as_bytes());
    let seed = root.join("seed.txt");
    fs::write(&seed, "other::fails\n").unwrap();
    let seeded = report(SHA, START, END, root, &seed).unwrap();
    assert_eq!(
        seeded + "\n",
        expected.replace(
            "Seed set: absent; classification unavailable",
            &format!(
                "Seed set: {}\nNames absent from seed set: 0; signatures still require review",
                seed.display()
            )
        )
    );
}

#[test]
fn verdict_green_known_red_unknown_and_clean_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    collected(root, 7, &["a::known", "b::other"]);
    gzip(&root.join("failure-logs/other-05.log.gz"), FAIL_LOG);
    // Full log wins over a contradictory excerpt.
    fs::write(
        root.join("failure-logs/other-05.txt"),
        "FAIL [ 00:01 ] (1/1) test false::excerpt\n",
    )
    .unwrap();
    let snap = classifier(root, &["a::known", "b::other"]);
    assert_eq!(
        run_verdict(root, &snap, true),
        format!(
            "\n## Verdict\n\n- KNOWN #7 `a::known`\n- KNOWN #7 `b::other`\nKnown only: #7\n\nVERDICT GREEN {SHA} new=0\n"
        )
    );
    collected(root, 7, &["a::known", "b::other"]);
    let snap = classifier(root, &["a::known"]);
    assert_eq!(
        run_verdict(root, &snap, true),
        format!(
            "\n## Verdict\n\n- KNOWN #7 `a::known`\n- NEW `b::other`\n\nVERDICT RED {SHA} new=1\n"
        )
    );
    collected(root, 7, &[]);
    assert_eq!(
        run_verdict(root, &snap, true),
        format!("\n## Verdict\n\n- no failing test names\n\nVERDICT GREEN {SHA} new=0\n")
    );
}

#[test]
fn verdict_incomplete_missing_classifier_snapshot_and_name_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let snap = classifier(root, &[]);
    collected(root, 7, &["other-05:build_or_harness_failure"]);
    assert_eq!(
        run_verdict(root, &snap, true),
        format!(
            "\n## Verdict\n\nIncomplete: other-05:build_or_harness_failure\n- no failing test names\n\nVERDICT INCOMPLETE {SHA} new=0\n"
        )
    );
    collected(root, 0, &[]);
    assert_eq!(
        run_verdict(root, &snap, true),
        format!(
            "\n## Verdict\n\nIncomplete: Lanes: 0\n- no failing test names\n\nVERDICT INCOMPLETE {SHA} new=0\n"
        )
    );
    collected(root, 7, &["a::known"]);
    assert_eq!(
        run_verdict(root, &snap, false),
        format!(
            "\n## Verdict\n\nClassification unavailable: rsi-known-failure CLI not found; reds are unclassified.\n- NEW `a::known`\n\nVERDICT RED {SHA} new=1\n"
        )
    );
    collected(root, 7, &["a::known"]);
    let missing = root.join("absent.json");
    assert_eq!(
        run_verdict(root, &missing, true),
        format!(
            "\n## Verdict\n\nClassification unavailable: snapshot {} missing; reds are unclassified.\n- NEW `a::known`\n\nVERDICT RED {SHA} new=1\n",
            missing.display()
        )
    );
    let qa = fs::read_to_string(root.join("QA.md")).unwrap();
    let matches = BTreeMap::from([("a::known".into(), ("name-only".into(), "#7,#8".into()))]);
    assert_eq!(
        build_verdict(&qa, SHA, Classification::Ok(matches)),
        format!(
            "\n## Verdict\n\n- NEW `a::known` (name-only match #7,#8; signature unverified)\n\nVERDICT RED {SHA} new=1\n"
        )
    );
}

#[test]
fn crash_and_timeout_remain_named_reds_and_unnamed_signal_is_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lane(root, "rsid-session-04", 100, CRASH_LOG);
    write_lane(root, "rsid-store-01", 0, "all green\n");
    let qa = human(root);
    assert_eq!(
        qa,
        include_str!("fixtures/sweep-crash-report.md.in").replace("$BR", "  ")
    );
    assert_eq!(
        rsi_cloud::sweep::failures(CRASH_LOG),
        vec![CRASHED.to_owned(), "session::tests::timed_out".into()]
    );
    fs::write(root.join("QA.md"), qa).unwrap();
    assert_eq!(
        run_verdict(root, &root.join("snapshot.json"), false),
        format!(
            "\n## Verdict\n\nClassification unavailable: rsi-known-failure CLI not found; reds are unclassified.\n- NEW `{CRASHED}` [crash]\n- NEW `session::tests::timed_out` [timeout]\nCrashed or timed out (NEW): 2; the verdict is RED, not INCOMPLETE.\n\nVERDICT RED {SHA} new=2\n"
        )
    );
    let other = tempfile::tempdir().unwrap();
    write_lane(
        other.path(),
        "other-doctests",
        101,
        "error: process didn't exit successfully: `x` (signal: 6, SIGABRT: process abort signal)\n",
    );
    let qa = human(other.path());
    assert!(qa.contains("- `other-doctests:crash`\n"));
    assert!(qa.contains("- `other-doctests:crash` [crash]\n"));
}

#[test]
fn excerpt_fallback_classifier_failure_and_corrupt_gzip_stay_red() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let snap = classifier(root, &["a::known"]);
    collected(root, 7, &["a::known"]);
    fs::write(root.join("failure-logs/lane.txt"), FAIL_LOG).unwrap();
    assert!(run_verdict(root, &snap, true).ends_with(&format!("VERDICT GREEN {SHA} new=0\n")));
    fs::write(root.join("failure-logs/lane.log.gz"), "not gzip").unwrap();
    collected(root, 7, &["a::known"]);
    let section = run_verdict(root, &snap, true);
    assert!(section.contains("Classification unavailable: classifier failed:"));
    assert!(section.ends_with(&format!("VERDICT RED {SHA} new=1\n")));
    fs::remove_file(root.join("failure-logs/lane.log.gz")).unwrap();
    fs::write(&snap, "FAIL_CLASSIFIER").unwrap();
    collected(root, 7, &["a::known"]);
    assert!(run_verdict(root, &snap, true).ends_with(&format!("VERDICT RED {SHA} new=1\n")));
}

#[cfg(unix)]
#[test]
fn failed_lane_log_excerpts_survive_collect() {
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_lane(
        root,
        "other-05",
        100,
        "noise\nFAIL [ 00:01 ] (1/1) sandbox::t::shared\nthread 'x' panicked at src/t.rs:1:1:\nassertion failed\n",
    );
    write_lane(root, "session-01", 0, "all green\n");
    let packed = Command::new("bash")
        .arg(common::repo_root().join("scripts/cloud-sweep-excerpts.sh"))
        .arg(root)
        .output()
        .unwrap();
    assert!(packed.status.success());
    let unpacked = root.join("unpacked");
    fs::create_dir(&unpacked).unwrap();
    let mut tar = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(&unpacked)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    tar.stdin.take().unwrap().write_all(&packed.stdout).unwrap();
    assert!(tar.wait().unwrap().success());
    let mut names: Vec<_> = fs::read_dir(&unpacked)
        .unwrap()
        .map(|p| p.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["other-05.log.gz", "other-05.txt"]);
    let text = fs::read_to_string(unpacked.join("other-05.txt")).unwrap();
    for needle in [
        "lane=other-05 exit=100",
        "sandbox::t::shared",
        "assertion failed",
    ] {
        assert!(text.contains(needle));
    }
    let mut text = String::new();
    flate2::read::GzDecoder::new(fs::File::open(unpacked.join("other-05.log.gz")).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.contains("sandbox::t::shared"));
}

#[test]
fn argument_errors_keep_script_exit_codes_and_options_keep_their_order() {
    for command in ["sweep-json", "sweep-report"] {
        assert_eq!(common::run(&[command], "").code, 1);
    }
    assert_eq!(common::run(&["sweep-verdict"], "").code, 2);
    assert_eq!(common::run(&["sweep-verdict", "--help"], "").code, 0);
    assert_eq!(common::run(&["sweep-verdict", "--snapshot"], "").code, 2);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    collected(root, 7, &[]);
    let output = common::rsi_cloud()
        .args(["sweep-verdict", "--classifier=", "--snapshot=absent"])
        .arg(root)
        .env("RSI_KNOWN_FAILURE_BIN", root.join("missing-classifier"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "\n## Verdict\n\nClassification unavailable: rsi-known-failure CLI not found; reds are unclassified.\n- no failing test names\n\nVERDICT GREEN {SHA} new=0\n"
        )
    );
}
