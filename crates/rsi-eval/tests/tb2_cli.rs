//! Exercise the installed command family, exit codes, stdin, and proposer rehearsal.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn repo() -> PathBuf {
    PathBuf::from(rsi_common::manifest_dir!()).join("../..")
}
fn eval(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rsi-eval"))
        .current_dir(root)
        .args(["bench"])
        .args(args)
        .output()
        .unwrap()
}
fn code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_help_diff_stdin_redaction_and_exit_codes() {
    for command in ["leak-audit", "pin-check", "split", "refresh-dataset"] {
        code(&eval(&repo(), &["tb2", command, "--help"]), 0);
    }
    code(&eval(&repo(), &["tb2", "leak-audit"]), 2);
    code(&eval(&repo(), &["tb2", "pin-check", "missing-checkout"]), 2);
    let tmp = tempfile::tempdir().unwrap();
    let clean = tmp.path().join("clean.md");
    let dirty = tmp.path().join("dirty.md");
    fs::write(&clean, "work carefully\n").unwrap();
    fs::write(&dirty, "fine\nregex-log\n").unwrap();
    let out = eval(
        &repo(),
        &["tb2", "leak-audit", "--files", clean.to_str().unwrap()],
    );
    code(&out, 0);
    assert_eq!(out.stdout, b"leak_audit: clean\n");
    let out = eval(
        &repo(),
        &["tb2", "leak-audit", "--files", dirty.to_str().unwrap()],
    );
    code(&out, 1);
    assert!(String::from_utf8_lossy(&out.stdout).contains("dirty.md:2 task-name\n"));
    // The complete finding has only location and kind, even when the input contains a task name.
    assert_eq!(
        String::from_utf8(out.stdout)
            .unwrap()
            .split_whitespace()
            .count(),
        3
    );
    code(
        &eval(
            &repo(),
            &[
                "tb2",
                "leak-audit",
                "--files",
                tmp.path().join("missing.md").to_str().unwrap(),
            ],
        ),
        2,
    );
    let diff = "--- a/prompts/system.md\n+++ b/prompts/system.md\n@@ -0,0 +1 @@\n+path-tracing\n";
    let file = tmp.path().join("c.diff");
    fs::write(&file, diff).unwrap();
    let out = eval(
        &repo(),
        &["tb2", "leak-audit", "--diff", file.to_str().unwrap()],
    );
    code(&out, 1);
    assert_eq!(out.stdout, b"HIT prompts/system.md:1 task-name\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rsi-eval"))
        .current_dir(repo())
        .args(["bench", "tb2", "leak-audit", "--diff", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    let stdin = child.wait_with_output().unwrap();
    code(&stdin, 1);
    assert_eq!(stdin.stdout, out.stdout);
}

#[test]
fn native_pin_cli_clean_mismatch_and_bad_checkout() {
    use rsi_eval::tb2::dataset;

    // The fixture repository is created with `git init` inside its own
    // directory, so no enclosing checkout is ever consulted. The environment is
    // cleared so inherited repository locations (GIT_DIR and friends) and the
    // host's git config cannot leak in. This test deliberately does not use
    // `rsi-repo-guards`: nothing the daemon links may depend on it (#451).
    let tmp = tempfile::Builder::new()
        .prefix("rsi-native-pin-cli-")
        .tempdir()
        .unwrap();
    let checkout = tmp.path().join("checkout");
    let task = checkout.join("alpha/task.toml");
    fs::create_dir_all(task.parent().unwrap()).unwrap();
    fs::write(&task, "x=1\n").unwrap();
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command.current_dir(&checkout).env_clear();
        for key in ["PATH", "SYSTEMROOT", "SystemRoot", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args(args);
        let output = command.output().unwrap();
        code(&output, 0);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q", "--template="]);
    git(&["add", "alpha/task.toml"]);
    git(&[
        "-c",
        "user.name=pin-fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        "pin fixture",
    ]);
    let head = git(&["rev-parse", "--verify", "HEAD"]);
    let mut t = dataset::load(&repo().join("eval/bench/tb2/dataset.toml"))
        .unwrap()
        .task
        .remove(0);
    t.name = "alpha".into();
    t.task_toml_sha256 = dataset::task_hash(&task).unwrap();
    let dataset = tmp.path().join("dataset.toml");
    let pinned = dataset::render(&[t], &[]).replacen(dataset::COMMIT, &head, 1);
    fs::write(&dataset, &pinned).unwrap();
    let run = |root: &Path| {
        eval(
            &repo(),
            &[
                "tb2",
                "pin-check",
                root.to_str().unwrap(),
                "--dataset",
                dataset.to_str().unwrap(),
            ],
        )
    };
    let clean = run(&checkout);
    code(&clean, 0);
    assert_eq!(
        String::from_utf8(clean.stdout).unwrap(),
        format!("pin_check: ok (1 tasks at {})\n", &head[..8])
    );
    let wrong_commit = "0000000000000000000000000000000000000000";
    fs::write(&dataset, pinned.replacen(&head, wrong_commit, 1)).unwrap();
    let mismatch = run(&checkout);
    code(&mismatch, 1);
    assert_eq!(
        String::from_utf8(mismatch.stdout).unwrap(),
        format!("DRIFT checkout commit {head} is not the pinned {wrong_commit}\n")
    );
    fs::write(&dataset, &pinned).unwrap();
    fs::write(task, "x=2\n").unwrap();
    let drift = run(&checkout);
    code(&drift, 1);
    assert!(String::from_utf8_lossy(&drift.stdout).starts_with("DRIFT task.toml changed: alpha"));
    let plain = tmp.path().join("plain");
    fs::create_dir(&plain).unwrap();
    let bad_checkout = run(&plain);
    code(&bad_checkout, 2);
    assert!(
        String::from_utf8_lossy(&bad_checkout.stderr)
            .contains("checkout has no .git; cannot verify the pinned commit")
    );
    code(&run(&tmp.path().join("missing")), 2);
}

fn candidate(root: &Path, args: &[&str]) -> Output {
    let mut command = vec!["candidate", "--root", root.to_str().unwrap()];
    command.extend(args);
    eval(root, &command)
}
fn id(output: &Output) -> String {
    code(output, 0);
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["candidate"]
        .as_str()
        .unwrap()
        .into()
}
fn notes(parent: &str, cited: &str) -> String {
    format!(
        "# Rehearsal\n\n## Parent\n{parent}\n\n## Hypothesis\nInspect the fixture digests before acting.\n\n## Single change\nCheck local dependencies first.\n\n## Expected effect\nFewer tool errors and wasted turns.\n\n## Prior candidates cited\n{cited}\n\n## Risk\nAn extra inspection could cost a turn.\n"
    )
}

#[test]
fn candidate_verify_passes_the_committed_proposer_dry_run_flow_natively() {
    let tmp = tempfile::tempdir().unwrap();
    let tb2 = tmp.path().join("eval/bench/tb2");
    fs::create_dir_all(&tb2).unwrap();
    for file in ["dataset.toml", "split.json"] {
        fs::copy(repo().join("eval/bench/tb2").join(file), tb2.join(file)).unwrap();
    }
    let seed = id(&candidate(tmp.path(), &["new", "--seed"]));
    let dir = tb2.join("candidates").join(&seed);
    fs::write(dir.join("notes.md"), notes("none", "none")).unwrap();
    fs::create_dir(dir.join("digest")).unwrap();
    for file in fs::read_dir(repo().join("eval/bench/tb2/proposer/fixtures/digest")).unwrap() {
        let file = file.unwrap();
        fs::copy(file.path(), dir.join("digest").join(file.file_name())).unwrap();
    }
    code(&candidate(tmp.path(), &["verify", &seed]), 0);
    code(&candidate(tmp.path(), &["index"]), 0);
    let mut earlier = Vec::new();
    let mut parent = seed;
    let export = tmp.path().join("events.json");
    fs::write(&export, r#"{"events":[]}"#).unwrap();
    for instruction in [
        "Check the local tools before editing.\n",
        "Check the environment and read the task carefully.\n",
        "Inspect the local tools and verify the result.\n",
    ] {
        let child = id(&candidate(tmp.path(), &["new", "--parent", &parent]));
        let dir = tb2.join("candidates").join(&child);
        fs::write(
            dir.join("overlay.toml"),
            "instructions = \"instructions.md\"\n",
        )
        .unwrap();
        fs::write(dir.join("instructions.md"), instruction).unwrap();
        let child = id(&candidate(tmp.path(), &["rename", &child]));
        let dir = tb2.join("candidates").join(&child);
        let cited = if earlier.is_empty() {
            "none".into()
        } else {
            earlier.join(", ")
        };
        fs::write(dir.join("notes.md"), notes(&parent, &cited)).unwrap();
        let verified = candidate(
            tmp.path(),
            &[
                "verify",
                &child,
                "--proposer-events",
                export.to_str().unwrap(),
            ],
        );
        code(&verified, 0);
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("verify.json")).unwrap()).unwrap();
        assert_eq!(report["ok"], true);
        assert_eq!(report["read_audit_ran"], true);
        code(&candidate(tmp.path(), &["index"]), 0);
        earlier.push(child.clone());
        parent = child;
    }
    let selection = eval(
        &repo(),
        &[
            "select",
            "eval/bench/tb2/proposer/fixtures/select-state.json",
        ],
    );
    code(&selection, 10);
}
