//! The committed dry-run fixture of the Meta-Harness proposer skill (#3197, program #3135 M6).
//!
//! `eval/bench/tb2/proposer/SKILL.md` lets a proposer rehearse one iteration before any paid screen
//! without a calibration record (#3184). This module builds that rehearsal input deterministically
//! and checks the committed copy against it:
//!
//! ```text
//! eval/bench/tb2/proposer/fixtures/
//!   record.json                  a BenchRunRecord v1: 6 search tasks, k=2, mixed pass and fail
//!   bench/<run>/<trial>/trace.jsonl   the traces the digests name (made-up, mechanical)
//!   digest/                      what `rsi-eval bench digest` writes from the record
//!   outcomes/screen-*.json       outcome records of three imaginary screens (no traces)
//!   select-state.json            the `rsi-eval bench select` state over those outcomes
//! ```
//!
//! The digest tool names the absolute trace path of the machine that ran it. A committed copy would
//! be wrong on every other checkout, so the generator rewrites that prefix to the repository-relative
//! `eval/bench/tb2/proposer/fixtures/bench`, which a proposer greps from the repository root. The
//! fixture sits outside `eval/bench/tb2/records/`, so the proposer read audit accepts those paths.
//!
//! Regenerate after a deliberate change: `RSI_PROPOSER_FIXTURE_BLESS=1 cargo test -p rsi-eval --lib
//! -- --exact proposer_fixture::tests::the_committed_fixture_matches_the_generator`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rsi_common::bench_record::{
    BenchRunRecord, EffectiveInputs, RunInfo, SCHEMA_VERSION, Timeouts, Totals, TrialRow,
};
use serde_json::{Value, json};

use crate::bench_digest::{DigestArgs, run_digest};

/// Repository-relative root of the fixture.
const FIXTURE_REL: &str = "eval/bench/tb2/proposer/fixtures";
/// The run directory name under `bench/`.
const RUN: &str = "fixture-baseline";
/// Six search-split tasks. No held-out task appears anywhere in the fixture.
const TASKS: [&str; 6] = [
    "fix-git",
    "openssl-selfsigned-cert",
    "password-recovery",
    "regex-chess",
    "db-wal-recovery",
    "git-multibranch",
];

/// One tool step of a made-up trace: `(tool, args, success, output, error)`.
type Step = (
    &'static str,
    Value,
    bool,
    &'static str,
    Option<&'static str>,
);

struct Trial {
    task: &'static str,
    attempt: u32,
    passed: bool,
    failure_cause: Option<&'static str>,
    stop_reason: &'static str,
    steps: Vec<Step>,
    first_message: &'static str,
    final_message: &'static str,
}

fn sh(command: &str, ok: bool, output: &'static str, error: Option<&'static str>) -> Step {
    ("shell", json!({ "command": command }), ok, output, error)
}

/// The twelve trials: 5 pass and 7 fail, with failures of different shapes (no environment survey,
/// a repeated failing command, an assumed tool, a claimed success that was never checked).
fn trials() -> Vec<Trial> {
    vec![
        Trial {
            task: "fix-git",
            attempt: 0,
            passed: true,
            failure_cause: None,
            stop_reason: "stop",
            first_message: "Recover the lost commit on the repository in the working directory.",
            final_message: "Restored the commit with git reflog and cherry-pick, then checked git log.",
            steps: vec![
                sh(
                    "pwd && ls -la && git status",
                    true,
                    "/app\n.git\nnotes.txt\nOn branch master",
                    None,
                ),
                sh(
                    "git reflog | head -5",
                    true,
                    "a1b2c3d HEAD@{0}: checkout: moving to master\n9f8e7d6 HEAD@{1}: commit: add notes",
                    None,
                ),
                sh(
                    "git cherry-pick 9f8e7d6",
                    true,
                    "[master 4c5d6e7] add notes",
                    None,
                ),
                sh(
                    "git log --oneline | head -3",
                    true,
                    "4c5d6e7 add notes\na1b2c3d init",
                    None,
                ),
            ],
        },
        Trial {
            task: "fix-git",
            attempt: 1,
            passed: true,
            failure_cause: None,
            stop_reason: "stop",
            first_message: "Recover the lost commit on the repository in the working directory.",
            final_message: "Merged the detached commit back and confirmed the file is present.",
            steps: vec![
                sh(
                    "ls -la && git branch -a",
                    true,
                    ".git\nnotes.txt\n* master",
                    None,
                ),
                sh(
                    "git fsck --lost-found | head",
                    true,
                    "dangling commit 9f8e7d6",
                    None,
                ),
                sh(
                    "git merge 9f8e7d6",
                    true,
                    "Fast-forward\n notes.txt | 1 +",
                    None,
                ),
                sh("cat notes.txt", true, "release notes", None),
            ],
        },
        Trial {
            task: "openssl-selfsigned-cert",
            attempt: 0,
            passed: false,
            failure_cause: Some("tests_failed"),
            stop_reason: "stop",
            first_message: "Create a self-signed certificate and put the files where the task says.",
            final_message: "Generated the key and certificate in the working directory.",
            steps: vec![
                sh(
                    "openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 365 -subj /CN=localhost",
                    true,
                    "writing new private key to 'key.pem'",
                    None,
                ),
                sh("ls -la", true, "cert.pem\nkey.pem", None),
                sh(
                    "openssl x509 -in cert.pem -noout -subject",
                    true,
                    "subject=CN = localhost",
                    None,
                ),
            ],
        },
        Trial {
            task: "openssl-selfsigned-cert",
            attempt: 1,
            passed: true,
            failure_cause: None,
            stop_reason: "stop",
            first_message: "Create a self-signed certificate and put the files where the task says.",
            final_message: "Wrote the files to the directory the task names and verified the subject.",
            steps: vec![
                sh(
                    "pwd && ls -la / /app 2>&1 | head -20",
                    true,
                    "/app\n/app:\ncerts",
                    None,
                ),
                sh(
                    "mkdir -p /app/certs && openssl req -x509 -newkey rsa:2048 -nodes -keyout /app/certs/key.pem -out /app/certs/cert.pem -days 365 -subj /CN=localhost",
                    true,
                    "ok",
                    None,
                ),
                sh(
                    "openssl x509 -in /app/certs/cert.pem -noout -subject",
                    true,
                    "subject=CN = localhost",
                    None,
                ),
            ],
        },
        Trial {
            task: "password-recovery",
            attempt: 0,
            passed: false,
            failure_cause: Some("agent_timeout"),
            stop_reason: "max_iterations",
            first_message: "Recover the password from the damaged archive.",
            final_message: "I ran out of iterations before the password was found.",
            steps: vec![
                sh(
                    "unzip -P guess1 data.zip",
                    false,
                    "incorrect password",
                    Some("Exit code: 82"),
                ),
                sh(
                    "unzip -P guess2 data.zip",
                    false,
                    "incorrect password",
                    Some("Exit code: 82"),
                ),
                sh(
                    "unzip -P guess3 data.zip",
                    false,
                    "incorrect password",
                    Some("Exit code: 82"),
                ),
                sh(
                    "unzip -P guess4 data.zip",
                    false,
                    "incorrect password",
                    Some("Exit code: 82"),
                ),
                sh(
                    "unzip -P guess5 data.zip",
                    false,
                    "incorrect password",
                    Some("Exit code: 82"),
                ),
            ],
        },
        Trial {
            task: "password-recovery",
            attempt: 1,
            passed: false,
            failure_cause: Some("tests_failed"),
            stop_reason: "stop",
            first_message: "Recover the password from the damaged archive.",
            final_message: "The password is probably hunter2.",
            steps: vec![
                sh(
                    "ls -la && strings data.zip | head",
                    true,
                    "data.zip\nPK\nsecret.txt",
                    None,
                ),
                sh(
                    "fcrackzip -u -D -p /usr/share/wordlists/rockyou.txt data.zip",
                    false,
                    "bash: fcrackzip: command not found",
                    Some("Exit code: 127"),
                ),
            ],
        },
        Trial {
            task: "regex-chess",
            attempt: 0,
            passed: false,
            failure_cause: Some("tests_failed"),
            stop_reason: "stop",
            first_message: "Write the regex file the task describes.",
            final_message: "Wrote re.json.",
            steps: vec![
                sh(
                    "rustc gen.rs -o gen && ./gen > re.json",
                    false,
                    "bash: rustc: command not found",
                    Some("Exit code: 127"),
                ),
                sh(
                    "cargo run --quiet > re.json",
                    false,
                    "bash: cargo: command not found",
                    Some("Exit code: 127"),
                ),
                sh("echo '[]' > re.json", true, "", None),
            ],
        },
        Trial {
            task: "regex-chess",
            attempt: 1,
            passed: false,
            failure_cause: Some("tests_failed"),
            stop_reason: "stop",
            first_message: "Write the regex file the task describes.",
            final_message: "Wrote re.json with a minimal pattern list.",
            steps: vec![
                sh(
                    "rustc --version",
                    false,
                    "bash: rustc: command not found",
                    Some("Exit code: 127"),
                ),
                sh("echo '[[\"^$\",\"\"]]' > re.json", true, "", None),
            ],
        },
        Trial {
            task: "db-wal-recovery",
            attempt: 0,
            passed: true,
            failure_cause: None,
            stop_reason: "stop",
            first_message: "Recover the rows from the write-ahead log of the database.",
            final_message: "Replayed the log into a copy of the database and counted the rows.",
            steps: vec![
                sh(
                    "ls -la /app && which sqlite3 rustc",
                    true,
                    "/app:\nmain.db\nmain.db-wal\n/usr/bin/sqlite3\n/usr/bin/rustc",
                    None,
                ),
                sh(
                    "sqlite3 main.db 'select count(*) from items'",
                    true,
                    "3",
                    None,
                ),
                sh(
                    "cp main.db main.db-wal /tmp/ && sqlite3 /tmp/main.db 'pragma wal_checkpoint(full); select count(*) from items'",
                    true,
                    "0|2|2\n5",
                    None,
                ),
            ],
        },
        Trial {
            task: "db-wal-recovery",
            attempt: 1,
            passed: false,
            failure_cause: Some("tests_failed"),
            stop_reason: "stop",
            first_message: "Recover the rows from the write-ahead log of the database.",
            final_message: "Done: the database is recovered.",
            steps: vec![
                sh("ls -la /app", true, "main.db\nmain.db-wal", None),
                sh("rm main.db-wal", true, "", None),
            ],
        },
        Trial {
            task: "git-multibranch",
            attempt: 0,
            passed: false,
            failure_cause: Some("tests_failed"),
            stop_reason: "stop",
            first_message: "Configure the repository and hooks the task describes.",
            final_message: "Set up the hook and pushed once.",
            steps: vec![
                sh(
                    "git init --bare repo.git",
                    true,
                    "Initialized empty Git repository",
                    None,
                ),
                sh(
                    "echo 'echo deployed' > repo.git/hooks/post-receive",
                    true,
                    "",
                    None,
                ),
                sh(
                    "git push origin main",
                    false,
                    "fatal: 'origin' does not appear to be a git repository",
                    Some("Exit code: 128"),
                ),
            ],
        },
        Trial {
            task: "git-multibranch",
            attempt: 1,
            passed: true,
            failure_cause: None,
            stop_reason: "stop",
            first_message: "Configure the repository and hooks the task describes.",
            final_message: "Created the bare repository, made the hook executable and pushed both branches.",
            steps: vec![
                sh(
                    "pwd && ls -la && git --version",
                    true,
                    "/app\ngit version 2.43.0",
                    None,
                ),
                sh(
                    "git init --bare repo.git && chmod +x repo.git/hooks/post-receive",
                    false,
                    "chmod: cannot access 'repo.git/hooks/post-receive'",
                    Some("Exit code: 1"),
                ),
                sh(
                    "printf '#!/bin/sh\\necho deployed\\n' > repo.git/hooks/post-receive && chmod +x repo.git/hooks/post-receive",
                    true,
                    "",
                    None,
                ),
                sh(
                    "git push repo.git main dev",
                    true,
                    "To repo.git\n * [new branch] main -> main\n * [new branch] dev -> dev",
                    None,
                ),
            ],
        },
    ]
}

fn trace_lines(trial: &Trial) -> Vec<Value> {
    let mut lines = Vec::new();
    let mut seq = 0_u64;
    let push = |value: Value, lines: &mut Vec<Value>, seq: &mut u64| {
        let mut value = value;
        value["seq"] = json!(*seq);
        *seq += 1;
        lines.push(value);
    };
    push(
        json!({"kind": "model_request", "request": {"model": "fixture-model", "messages": [
            {"role": "system", "content": "fixture bench prompt"},
            {"role": "user", "content": trial.first_message}], "tools": [{}, {}]}}),
        &mut lines,
        &mut seq,
    );
    for (tool, args, ok, output, error) in &trial.steps {
        push(
            json!({"kind": "tool_call", "name": tool, "args": args}),
            &mut lines,
            &mut seq,
        );
        push(
            json!({"kind": "tool_result", "name": tool,
                "result": {"success": ok, "output": output, "error_msg": error}}),
            &mut lines,
            &mut seq,
        );
    }
    push(
        json!({"kind": "model_response", "response": {
            "content": trial.final_message, "tool_calls": [], "stop_reason": trial.stop_reason,
            "usage": {"prompt_tokens": 900, "completion_tokens": 40, "cost_usd": 0.004}}}),
        &mut lines,
        &mut seq,
    );
    lines
}

fn trace_text(trial: &Trial) -> String {
    let mut text = String::new();
    for line in trace_lines(trial) {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    text
}

fn trial_dir_name(trial: &Trial) -> String {
    format!("{}__a{}", trial.task, trial.attempt)
}

fn sha256_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn row(trial: &Trial, with_trace: bool, overlay: &str) -> TrialRow {
    TrialRow {
        attempt: trial.attempt,
        build_sha: "b".repeat(40),
        cached_tokens: 100,
        cost_usd: 0.01,
        exception_type: None,
        failure_cause: trial.failure_cause.map(str::to_string),
        harness_source_digest: "sha256:fixture-source".to_string(),
        harness_version_hash: format!("fixture-{}-{overlay}", trial.task),
        image_digest: "sha256:fixture-image".to_string(),
        infra_interrupted: false,
        invalid_route: false,
        output_tokens: 40,
        passed: trial.passed,
        prompt_tokens: 900,
        read_output_calls: 0,
        reasoning_tokens: 0,
        reward: f64::from(u8::from(trial.passed)),
        reward_valid: true,
        session_id: Some(format!("fixture-session-{}-{}", trial.task, trial.attempt)),
        spills: 0,
        split: "search".to_string(),
        stop_reason: Some(trial.stop_reason.to_string()),
        suspected_solution_lookup: false,
        task: trial.task.to_string(),
        trace_sha256: with_trace.then(|| sha256_of(&trace_text(trial))),
        turns: u32::try_from(trial.steps.len() + 1).unwrap(),
        wall_ms: 60_000,
    }
}

fn record(trials: Vec<TrialRow>, overlay: &str) -> BenchRunRecord {
    let mut record = BenchRunRecord {
        run: RunInfo {
            adapter_tree_hash: "fixture-tree".to_string(),
            build_sha: "b".repeat(40),
            concurrency: 1,
            config_digest: "sha256:fixture-config".to_string(),
            dataset_commit: "2fd12b88aafdd04a52c298e3940bcb189f9766d6".to_string(),
            dataset_repo: "harbor-framework/terminal-bench-2".to_string(),
            effective_inputs: EffectiveInputs {
                overlay: overlay.to_string(),
                ..EffectiveInputs::default()
            },
            effort: "high".to_string(),
            finished_at: "2026-10-19T13:00:00.000000000Z".to_string(),
            harbor_version: "0.24.0".to_string(),
            harness_build_digest: format!("sha256:fixture-build-{overlay}"),
            harness_source_digest: "sha256:fixture-source".to_string(),
            host: "fixture-host".to_string(),
            image_digests: BTreeMap::new(),
            k: 2,
            model: "fixture-model".to_string(),
            observation_fingerprint: String::new(),
            route: "direct".to_string(),
            sampling: BTreeMap::new(),
            split: "search".to_string(),
            started_at: "2026-10-19T12:00:00.000000000Z".to_string(),
            tasks: {
                let mut tasks: Vec<String> = TASKS.iter().map(|t| (*t).to_string()).collect();
                tasks.sort();
                tasks
            },
            timeouts: Timeouts {
                agent_sec: 900,
                multiplier: 1.0,
                verifier_sec: 600,
            },
            totals: Totals::default(),
        },
        schema_version: SCHEMA_VERSION,
        trials,
    };
    record.seal();
    record
}

/// An imaginary screen: the baseline's outcomes with `flips` trials changed, no traces.
fn screen(overlay: &str, flips: &[(&str, u32, bool)]) -> BenchRunRecord {
    let mut trials: Vec<TrialRow> = trials().iter().map(|t| row(t, false, overlay)).collect();
    for (task, attempt, passed) in flips {
        let trial = trials
            .iter_mut()
            .find(|t| t.task == *task && t.attempt == *attempt)
            .expect("flipped trial exists");
        trial.passed = *passed;
        trial.reward = f64::from(u8::from(*passed));
        trial.failure_cause = (!*passed).then(|| "tests_failed".to_string());
    }
    record(trials, overlay)
}

/// Every fixture file by repository-relative path below [`FIXTURE_REL`].
fn generate(scratch: &Path, repo_root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let bench = scratch.join("bench");
    let mut rows = Vec::new();
    for trial in trials() {
        let dir = bench.join(RUN).join(trial_dir_name(&trial));
        std::fs::create_dir_all(&dir).unwrap();
        let text = trace_text(&trial);
        std::fs::write(dir.join("trace.jsonl"), &text).unwrap();
        files.insert(
            format!("bench/{RUN}/{}/trace.jsonl", trial_dir_name(&trial)),
            text.into_bytes(),
        );
        rows.push(row(&trial, true, ""));
    }
    let baseline = record(rows, "");
    let record_path = scratch.join("record.json");
    std::fs::write(&record_path, baseline.to_json().unwrap()).unwrap();
    files.insert(
        "record.json".to_string(),
        baseline.to_json().unwrap().into_bytes(),
    );

    let out = scratch.join("digest");
    run_digest(&DigestArgs {
        record: record_path,
        out: out.clone(),
        split: repo_root.join("eval/bench/tb2/split.json"),
        bench_dir: bench.clone(),
        run: None,
        trajectories: None,
    })
    .unwrap();
    let absolute = bench.to_string_lossy().into_owned();
    let relative = format!("{FIXTURE_REL}/bench");
    for entry in std::fs::read_dir(&out).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace(&absolute, &relative);
        files.insert(
            format!("digest/{}", path.file_name().unwrap().to_string_lossy()),
            text.into_bytes(),
        );
    }

    let screens: [(&str, &[(&str, u32, bool)]); 3] = [
        (
            "sha256:fixture-screen-1",
            &[("regex-chess", 0, true), ("password-recovery", 0, true)],
        ),
        (
            "sha256:fixture-screen-2",
            &[("openssl-selfsigned-cert", 0, true), ("fix-git", 1, false)],
        ),
        (
            "sha256:fixture-screen-3",
            &[("db-wal-recovery", 1, true), ("git-multibranch", 0, true)],
        ),
    ];
    let mut candidates = Vec::new();
    for (index, (overlay, flips)) in screens.iter().enumerate() {
        let name = format!("screen-{}", index + 1);
        let record = screen(overlay, flips);
        files.insert(
            format!("outcomes/{name}.json"),
            record.to_json().unwrap().into_bytes(),
        );
        candidates.push(json!({
            "id": format!("c{:02}-fixture", index + 1),
            "screen": [format!("outcomes/{name}.json")],
            "verify_clean": true,
        }));
    }
    let state = json!({
        "baseline_records": ["record.json"],
        "candidates": candidates,
        "heldout_tasks": 59,
        "no_further_hypothesis": false,
        "search_tasks": 6,
        "spent_usd": 0.0,
    });
    files.insert(
        "select-state.json".to_string(),
        (serde_json::to_string_pretty(&state).unwrap() + "\n").into_bytes(),
    );
    files
}

fn repo_root() -> PathBuf {
    PathBuf::from(rsi_common::manifest_dir!()).join("../..")
}

fn committed_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated() -> BTreeMap<String, Vec<u8>> {
        let scratch = tempfile::tempdir().unwrap();
        generate(scratch.path(), &repo_root())
    }

    #[test]
    fn the_committed_fixture_matches_the_generator() {
        let fixture_dir = repo_root().join(FIXTURE_REL);
        let want = generated();
        if std::env::var_os("RSI_PROPOSER_FIXTURE_BLESS").is_some() {
            for (rel, bytes) in &want {
                let path = fixture_dir.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
            return;
        }
        let have = committed_files(&fixture_dir);
        assert_eq!(
            have.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "fixture file set differs; bless with RSI_PROPOSER_FIXTURE_BLESS=1"
        );
        for (rel, bytes) in &want {
            assert!(
                have[rel] == *bytes,
                "{rel} differs; bless with RSI_PROPOSER_FIXTURE_BLESS=1"
            );
        }
    }

    #[test]
    fn the_fixture_is_search_split_only_with_mixed_outcomes() {
        let split: Value = serde_json::from_str(
            &std::fs::read_to_string(repo_root().join("eval/bench/tb2/split.json")).unwrap(),
        )
        .unwrap();
        let search: Vec<&str> = split["search"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let held_out: Vec<&str> = split["held_out"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(TASKS.iter().all(|t| search.contains(t)));
        let files = generated();
        for (rel, bytes) in &files {
            let text = String::from_utf8_lossy(bytes);
            for name in &held_out {
                assert!(!text.contains(name), "{rel} names a held-out task");
            }
        }
        let trials = trials();
        let passed = trials.iter().filter(|t| t.passed).count();
        assert!(passed > 0 && passed < trials.len());
        assert!(
            trials
                .iter()
                .any(|t| t.task == "password-recovery" && !t.passed)
        );
    }

    #[test]
    fn every_digest_names_a_trace_the_fixture_holds_and_never_a_machine_path() {
        let files = generated();
        let digests: Vec<(&String, String)> = files
            .iter()
            .filter(|(rel, _)| rel.starts_with("digest/") && rel.ends_with(".md"))
            .map(|(rel, bytes)| (rel, String::from_utf8(bytes.clone()).unwrap()))
            .collect();
        assert_eq!(digests.len(), 12);
        for (rel, text) in digests {
            let line = text
                .lines()
                .find_map(|l| l.strip_prefix("- trace: "))
                .unwrap_or_else(|| panic!("{rel} names no trace"));
            let path = line.split(" (").next().unwrap();
            let inside = path
                .strip_prefix(&format!("{FIXTURE_REL}/"))
                .unwrap_or_else(|| panic!("{rel}: {path} is not a fixture path"));
            assert!(
                files.contains_key(inside),
                "{rel}: {inside} is not committed"
            );
            assert!(!text.contains(".rsi/bench"), "{rel}");
        }
    }

    fn skill_text() -> String {
        std::fs::read_to_string(repo_root().join("eval/bench/tb2/proposer/SKILL.md")).unwrap()
    }

    #[test]
    fn the_skill_names_only_candidate_commands_the_cli_accepts() {
        use clap::Parser;
        let text = skill_text();
        let mut seen = std::collections::BTreeSet::new();
        for line in text.lines() {
            let Some(rest) = line.trim().strip_prefix("rsi-eval bench candidate ") else {
                continue;
            };
            let rest = rest.split(" #").next().unwrap();
            let argv = std::iter::once("rsi-eval bench candidate")
                .chain(rest.split_whitespace())
                .map(str::to_string);
            let parsed = crate::candidate::CandidateCli::try_parse_from(argv);
            assert!(
                parsed.is_ok(),
                "SKILL.md line does not parse: {line}: {:?}",
                parsed.err()
            );
            seen.insert(rest.split_whitespace().next().unwrap().to_string());
        }
        let want: std::collections::BTreeSet<String> = ["new", "rename", "verify", "index"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn the_skill_forbids_the_records_and_names_no_task_of_either_split() {
        let text = skill_text();
        assert!(text.contains("eval/bench/tb2/records/"));
        assert!(text.contains("no_further_hypothesis"));
        assert!(text.contains("proposer/fixtures"));
        let split: Value = serde_json::from_str(
            &std::fs::read_to_string(repo_root().join("eval/bench/tb2/split.json")).unwrap(),
        )
        .unwrap();
        for list in ["search", "held_out"] {
            for name in split[list]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
            {
                let named = text.match_indices(name).any(|(at, _)| {
                    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
                    !text[..at].chars().next_back().is_some_and(word)
                        && !text[at + name.len()..].chars().next().is_some_and(word)
                });
                assert!(!named, "SKILL.md names a {list} task");
            }
        }
    }

    #[test]
    fn select_runs_on_the_fixture_outcomes_and_names_a_next_action() {
        let state = repo_root().join(FIXTURE_REL).join("select-state.json");
        let data = crate::bench_select::load_state(&state).unwrap();
        assert_eq!(data.candidates.len(), 3);
        let selection = crate::bench_select::select(&data, 1).unwrap();
        // Three eligible screens are below the plateau minimum of four: screen another candidate.
        assert_eq!(selection.action, crate::bench_select::Action::Propose);
        assert!(!crate::bench_select::render_markdown(&selection).is_empty());
    }
}
