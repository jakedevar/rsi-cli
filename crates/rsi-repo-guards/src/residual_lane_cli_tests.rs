use super::*;
use crate::testutil::{git, run as invoke, scratch_repository, write};

fn regression(changed: &str, rust: bool, red: bool) {
    let root = scratch_repository();
    let repo = root.path();
    git(repo, &["init", "-q"]);
    let suite = "scripts/tests/test_shell_tests.sh";
    write(
        repo,
        suite,
        "# suite-gate: tap\n# test_widget.sh covers widget.sh\nprintf '1..1\\nok 1 - wrapper\\n'\n",
    );
    write(repo, "scripts/tests/test_widget.sh", "exit 0\n");
    write(repo, "scripts/widget.sh", "exit 0\n");
    write(repo, "crates/demo/src/lib.rs", "pub fn value() {}\n");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]).trim().to_owned();
    if red {
        write(
            repo,
            suite,
            "# suite-gate: tap\nprintf '1..1\\nnot ok 1 - deliberately red\\n'\n",
        );
    } else {
        let text = std::fs::read_to_string(repo.join(changed)).unwrap();
        write(repo, changed, &format!("{text}# changed\n"));
    }
    if rust {
        write(
            repo,
            "crates/demo/src/lib.rs",
            "pub fn value() { let _changed = 1; }\n",
        );
    }
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "candidate"]);
    let head = git(repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let logs = tempfile::tempdir().unwrap();
    let path = logs.path().join("receipt.json");
    // TAP_SELECT must never narrow the selected suite in a landing gate.
    let ctx = Ctx::new(
        repo,
        &[
            ("PATH", "/usr/bin:/bin"),
            ("RSI_PID_ISOLATION", "off"),
            ("TAP_SELECT", "missing"),
        ],
    );
    let (code, output, error) = invoke(
        main,
        &[
            "--base",
            &base,
            "--head",
            &head,
            "--repo",
            repo.to_str().unwrap(),
            "--receipt",
            path.to_str().unwrap(),
            "--timeout-secs",
            "10",
        ],
        &ctx,
    );
    assert_eq!(code, i32::from(red), "{output}\n{error}");
    let receipt: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(receipt["base"], base);
    assert_eq!(receipt["receipt_head"], head);
    assert_eq!(receipt["selected"], json!([suite]));
    assert_eq!(receipt["ok"], !red);
    assert_eq!(receipt["residual"][0]["test_file"], suite);
    assert_eq!(receipt["residual"][0]["executed_tests"], 1);
    assert_eq!(receipt["residual"][0]["exit_code"], 0);
    assert_eq!(receipt["residual"][0]["failed_tests"], usize::from(red));
    assert_eq!(
        receipt["residual"][0]["status"],
        if red { "failed" } else { "passed" }
    );
    assert!(
        std::fs::read_to_string(logs.path().join("residual-0.log"))
            .unwrap()
            .contains("1..1")
    );
}

#[test]
fn shell_only_diff_runs_tap_suite() {
    regression("scripts/tests/test_shell_tests.sh", false, false);
}
#[test]
fn rust_and_changed_shell_test_add_wrapper() {
    regression("scripts/tests/test_widget.sh", true, false);
}
#[test]
fn rust_and_changed_covered_subject_add_wrapper() {
    regression("scripts/widget.sh", true, false);
}
#[test]
fn failing_tap_refuses_even_with_zero_shell_exit() {
    regression("scripts/tests/test_shell_tests.sh", true, true);
}
#[test]
fn empty_lane_and_selection_errors_both_write_receipts() {
    let root = scratch_repository();
    git(root.path(), &["init", "-q"]);
    write(root.path(), "docs/readme.md", "base\n");
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "base"]);
    let head = git(root.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let ctx = Ctx::new(root.path(), &[("PATH", "/usr/bin:/bin")]);
    let path = root.path().join("logs/receipt.json");
    for (base, ok) in [(head.as_str(), true), ("missing-commit", false)] {
        let (code, _, _) = invoke(
            main,
            &[
                "--repo",
                ".",
                "--base",
                base,
                "--head",
                &head,
                "--receipt",
                path.to_str().unwrap(),
            ],
            &ctx,
        );
        assert_eq!(code, i32::from(!ok));
        let receipt: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(receipt["ok"], ok);
        assert_eq!(receipt["selected"], json!([]));
        assert_eq!(receipt["error"].is_null(), ok);
    }
}
