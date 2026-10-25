#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::case_sensitive_file_extension_comparisons
)]

use std::collections::BTreeMap;

use rsi_common::bench_record::{EffectiveInputs, RunInfo, SCHEMA_VERSION, Timeouts, Totals};
use serde_json::{Value, json};

use super::*;

const RUN: &str = "run-1";

fn row(task: &str, attempt: u32, passed: bool) -> TrialRow {
    TrialRow {
        attempt,
        build_sha: "b".repeat(40),
        cached_tokens: 10,
        cost_usd: 0.5,
        exception_type: None,
        failure_cause: (!passed).then(|| "tests_failed".to_string()),
        harness_source_digest: "sha256:src".to_string(),
        harness_version_hash: format!("hvh-{task}"),
        image_digest: "sha256:aa".to_string(),
        infra_interrupted: false,
        invalid_route: false,
        output_tokens: 5,
        passed,
        prompt_tokens: 20,
        reasoning_tokens: 0,
        reward: f64::from(u8::from(passed)),
        reward_valid: true,
        session_id: Some(format!("sess-{task}-{attempt}")),
        split: "search".to_string(),
        stop_reason: Some("stop".to_string()),
        read_output_calls: 0,
        spills: 0,
        suspected_solution_lookup: false,
        task: task.to_string(),
        trace_sha256: None,
        turns: 3,
        wall_ms: 1000,
    }
}

fn record(trials: Vec<TrialRow>) -> BenchRunRecord {
    let mut tasks: Vec<String> = trials.iter().map(|t| t.task.clone()).collect();
    tasks.sort();
    tasks.dedup();
    let mut record = BenchRunRecord {
        run: RunInfo {
            adapter_tree_hash: "tree".to_string(),
            build_sha: "b".repeat(40),
            concurrency: 1,
            config_digest: "sha256:cfg".to_string(),
            dataset_commit: "2fd12b88".to_string(),
            dataset_repo: "harbor-framework/terminal-bench-2".to_string(),
            effective_inputs: EffectiveInputs::default(),
            effort: "high".to_string(),
            finished_at: "2026-10-19T13:00:00.000000000Z".to_string(),
            harbor_version: "0.24.0".to_string(),
            harness_build_digest: "sha256:build".to_string(),
            harness_source_digest: "sha256:src".to_string(),
            host: "h".to_string(),
            image_digests: BTreeMap::new(),
            k: 1,
            model: "m".to_string(),
            observation_fingerprint: String::new(),
            route: "direct".to_string(),
            sampling: BTreeMap::new(),
            split: "search".to_string(),
            started_at: "2026-10-19T12:00:00.000000000Z".to_string(),
            tasks,
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
    record.run.k = record
        .trials
        .iter()
        .map(|t| t.attempt + 1)
        .max()
        .unwrap_or(1);
    record.seal();
    record
}

struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("split.json"),
            r#"{"seed":1,"search":["alpha","beta","gamma"],"held_out":["secret"]}"#,
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Writes a trace and returns the sha256 the record row names.
    fn trace(&self, trial: &str, lines: &[Value]) -> String {
        let dir = self.path("bench").join(RUN).join(trial);
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        std::fs::write(dir.join("trace.jsonl"), &text).unwrap();
        hash_file(&dir.join("trace.jsonl")).unwrap()
    }

    fn args(&self, record: &BenchRunRecord, out: &str) -> DigestArgs {
        let path = self.path("record.json");
        std::fs::write(&path, record.to_json().unwrap()).unwrap();
        DigestArgs {
            record: path,
            out: self.path(out),
            split: self.path("split.json"),
            bench_dir: self.path("bench"),
            run: None,
            trajectories: None,
        }
    }
}

fn call(seq: u64, name: &str, args: &Value) -> Value {
    json!({"seq": seq, "kind": "tool_call", "name": name, "args": args})
}

fn result(seq: u64, name: &str, ok: bool, output: &str, error: Option<&str>) -> Value {
    json!({"seq": seq, "kind": "tool_result", "name": name,
        "result": {"success": ok, "output": output, "error_msg": error}})
}

fn request(seq: u64, messages: &[(&str, &str)]) -> Value {
    let messages: Vec<Value> = messages
        .iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    json!({"seq": seq, "kind": "model_request",
        "request": {"model": "m", "messages": messages, "tools": [{}, {}]}})
}

fn response(seq: u64, content: &str) -> Value {
    json!({"seq": seq, "kind": "model_response", "response": {
        "content": content, "tool_calls": [], "stop_reason": "stop",
        "usage": {"prompt_tokens": 100, "completion_tokens": 7, "cost_usd": 0.01}}})
}

/// A failing trace: two tool calls, the second exits 2, a compaction, then a final answer.
fn failing_trace() -> Vec<Value> {
    vec![
        request(0, &[("system", "sys"), ("user", "fix the thing")]),
        response(1, "I will look"),
        call(2, "shell", &json!({"command": "ls"})),
        result(3, "shell", true, "a.txt\nb.txt", None),
        call(4, "read_file", &json!({"path": "a.txt"})),
        result(5, "read_file", true, "hello", None),
        call(6, "shell", &json!({"command": "make"})),
        result(
            7,
            "shell",
            false,
            "make: *** [all] Error 2",
            Some("Exit code: 2"),
        ),
        request(
            8,
            &[
                ("system", "sys"),
                ("system", "[Conversation summary: did stuff]"),
            ],
        ),
        response(9, "giving up"),
    ]
}

fn read(world: &World, out: &str, name: &str) -> String {
    std::fs::read_to_string(world.path(out).join(name)).unwrap()
}

fn golden(world: &World, name: &str, expected: &str, actual: &str) {
    let actual = actual.replace(world.path("bench").to_str().unwrap(), "<BENCH>");
    if std::env::var_os("RSI_DIGEST_BLESS").is_some() {
        let path = rsi_common::manifest_dir!("/src/bench_digest/golden");
        std::fs::write(PathBuf::from(path).join(name), &actual).unwrap();
        return;
    }
    assert_eq!(
        actual, expected,
        "golden {name} differs; bless with RSI_DIGEST_BLESS=1"
    );
}

fn failed_world() -> (World, DigestArgs) {
    let world = World::new();
    let mut failed = row("alpha", 0, false);
    failed.trace_sha256 = Some(world.trace("alpha__x1", &failing_trace()));
    let mut passed = row("beta", 0, true);
    passed.trace_sha256 = Some(world.trace(
        "beta__y1",
        &[
            request(0, &[("user", "go")]),
            call(1, "shell", &json!({"command": "true"})),
            result(2, "shell", true, "ok", None),
            response(3, "done"),
        ],
    ));
    let args = world.args(&record(vec![failed, passed]), "out");
    (world, args)
}

#[test]
fn a_failed_trial_digest_matches_its_golden() {
    let (world, args) = failed_world();
    run_digest(&args).unwrap();
    golden(
        &world,
        "failed.k0.md",
        include_str!("golden/failed.k0.md"),
        &read(&world, "out", "alpha.k0.md"),
    );
}

#[test]
fn a_passed_trial_digest_matches_its_golden() {
    let (world, args) = failed_world();
    run_digest(&args).unwrap();
    golden(
        &world,
        "passed.k0.md",
        include_str!("golden/passed.k0.md"),
        &read(&world, "out", "beta.k0.md"),
    );
}

#[test]
fn rust_daemon_trial_names_resolve_the_recorded_trace_hash() {
    let world = World::new();
    let mut trial = row("alpha", 0, false);
    trial.trace_sha256 = Some(world.trace("alpha-s0-r0", &failing_trace()));
    let mut record = record(vec![trial]);
    record.run.harbor_version = rsi_common::bench_runner::RUNNER_VERSION_DISCRIMINATOR.into();
    record.seal();
    let args = world.args(&record, "out");
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    assert!(text.contains("alpha-s0-r0/trace.jsonl"), "{text}");
    assert!(text.contains("make: *** [all] Error 2"), "{text}");
}

#[test]
fn rust_collected_traces_select_the_matching_retry_hash() {
    let world = World::new();
    let mut trial = row("alpha", 0, false);
    for retry in 0..=1 {
        let dir = world
            .path("bench")
            .join(RUN)
            .join(format!("trials/alpha-s0-r{retry}/logs/agent"));
        std::fs::create_dir_all(&dir).unwrap();
        let trace = if retry == 1 {
            failing_trace()
        } else {
            vec![response(0, "other attempt")]
        };
        let body = trace
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let path = dir.join("rsi-trace.jsonl");
        std::fs::write(&path, body).unwrap();
        if retry == 1 {
            trial.trace_sha256 = Some(hash_file(&path).unwrap());
        }
    }
    let mut args = world.args(&record(vec![trial]), "out");
    args.run = Some(RUN.into());
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    assert!(
        text.contains("alpha-s0-r1/logs/agent/rsi-trace.jsonl"),
        "{text}"
    );
    assert!(text.contains("make: *** [all] Error 2"), "{text}");
}

#[test]
fn rust_trajectory_fallback_finds_the_matching_session_without_a_harbor_job() {
    let world = World::new();
    for retry in 0..=1 {
        let dir = world
            .path("bench")
            .join(RUN)
            .join(format!("trials/alpha-s0-r{retry}/logs/agent"));
        std::fs::create_dir_all(&dir).unwrap();
        let trajectory = json!({
            "session_id": if retry == 1 { "sess-alpha-0" } else { "other-session" },
            "steps": [{"step_id": 1, "source": "agent", "message": "Rust trajectory evidence"}]
        });
        std::fs::write(dir.join("trajectory.json"), trajectory.to_string()).unwrap();
    }
    let mut args = world.args(&record(vec![row("alpha", 0, false)]), "out");
    // Supplying the same run explicitly must not duplicate default discovery candidates.
    args.trajectories = Some(world.path("bench").join(RUN));
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    assert!(
        text.contains("alpha-s0-r1/logs/agent/trajectory.json"),
        "{text}"
    );
    assert!(text.contains("Rust trajectory evidence"), "{text}");
}

#[test]
fn the_trajectory_fallback_matches_its_golden() {
    let world = World::new();
    let trial = world
        .path("jobs")
        .join("job1")
        .join("alpha__t1")
        .join("agent");
    std::fs::create_dir_all(&trial).unwrap();
    let trajectory = json!({"session_id": "sess-alpha-0", "steps": [
        {"step_id": 1, "source": "user", "message": "fix the thing"},
        {"step_id": 2, "source": "agent", "message": "running", "model_name": "m",
         "tool_calls": [{"tool_call_id": "c1", "function_name": "shell", "arguments": {"command": "make"}}],
         "observation": {"results": [{"source_call_id": "c1", "content": "Exit code: 2"}]}},
        {"step_id": 3, "source": "agent", "message": "giving up"}]});
    std::fs::write(trial.join("trajectory.json"), trajectory.to_string()).unwrap();
    let mut args = world.args(&record(vec![row("alpha", 0, false)]), "out");
    args.trajectories = Some(world.path("jobs"));
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    assert!(text.contains("(trajectory.json)"), "{text}");
    assert!(
        text.contains("tool_result ERROR output=Exit code: 2"),
        "{text}"
    );
    golden(
        &world,
        "atif.k0.md",
        include_str!("golden/atif.k0.md"),
        &text.replace(world.path("jobs").to_str().unwrap(), "<BENCH>"),
    );
}

#[test]
fn a_digest_names_the_histogram_the_compaction_and_the_first_error_window() {
    let (world, args) = failed_world();
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    assert!(text.contains("- shell: 2\n- read_file: 1\n"), "{text}");
    assert!(text.contains("- #8 summary messages=2 -> 2 marker=[Conversation summary: did stuff]"));
    let errors = text.split("## First error").nth(1).unwrap();
    let errors = errors.split("## Last events").next().unwrap();
    // 5 events before the failed result, the result, and the 2 events that follow it.
    assert_eq!(
        errors.lines().filter(|l| l.starts_with("- #")).count(),
        8,
        "{errors}"
    );
    assert!(
        errors.contains("- #7 tool_result ERROR name=shell success=false error_msg=Exit code: 2")
    );
    assert!(errors.contains("- #2 tool_call name=shell"));
}

#[test]
fn only_the_last_thirty_events_are_kept() {
    let world = World::new();
    let mut lines = Vec::new();
    for i in 0..100 {
        lines.push(call(i, "shell", &json!({"n": i})));
    }
    let mut failed = row("alpha", 0, false);
    failed.trace_sha256 = Some(world.trace("alpha__x1", &lines));
    let args = world.args(&record(vec![failed]), "out");
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    let last = text.split("## Last events").nth(1).unwrap();
    assert_eq!(last.lines().filter(|l| l.starts_with("- #")).count(), 30);
    assert!(last.contains("- #70 tool_call") && last.contains("- #99 tool_call"));
    assert!(!last.contains("- #69 "));
    assert!(text.contains("- shell: 100"));
}

#[test]
fn a_field_is_cut_at_2000_characters_with_the_cut_marked() {
    let world = World::new();
    let big = "é".repeat(5_000);
    let mut failed = row("alpha", 0, false);
    failed.trace_sha256 = Some(world.trace("alpha__x1", &[result(0, "shell", true, &big, None)]));
    let args = world.args(&record(vec![failed]), "out");
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    let expected = format!("output={}…[+3000 chars]", "é".repeat(2_000));
    assert!(text.contains(&expected), "{}", &text[..text.len().min(600)]);
}

#[test]
fn trial_and_candidate_caps_hold_and_truncation_is_marked() {
    let world = World::new();
    let huge = "x".repeat(50_000);
    let mut trials = Vec::new();
    for task in ["alpha", "beta", "gamma"] {
        for attempt in 0..15 {
            let passed = attempt % 3 == 0;
            let mut r = row(task, attempt, passed);
            let lines: Vec<Value> = (0..60)
                .map(|i| result(i, "shell", i % 7 != 3, &huge, None))
                .collect();
            r.trace_sha256 = Some(world.trace(&format!("{task}__{attempt}"), &lines));
            trials.push(r);
        }
    }
    let args = world.args(&record(trials), "out");
    let outcome = run_digest(&args).unwrap();
    assert_eq!(outcome.trial_files, 45);
    let mut total = 0;
    for entry in std::fs::read_dir(&args.out).unwrap() {
        let path = entry.unwrap().path();
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        total += len;
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if name.ends_with(".md") {
            let passed = name.contains(".k0.") || name.contains(".k3.") || name.contains(".k6.");
            let cap = if passed {
                PASSED_TRIAL_CAP
            } else {
                FAILED_TRIAL_CAP
            };
            assert!(len <= cap, "{name} is {len} bytes, cap {cap}");
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                text.contains("…[+") || text.contains("[digest cut"),
                "{name}"
            );
        }
    }
    assert!(total <= CANDIDATE_CAP, "{total} bytes");
    assert_eq!(total, outcome.total_bytes);
    assert!(
        total > CANDIDATE_CAP / 2,
        "the budget is used, not wasted: {total}"
    );
}

#[test]
fn a_held_out_trial_is_refused_and_nothing_is_written() {
    let world = World::new();
    let mut held = row("secret", 0, true);
    held.trace_sha256 = Some(world.trace("secret__z1", &failing_trace()));
    let args = world.args(&record(vec![row("alpha", 0, false), held]), "out");
    let error = run_digest(&args).unwrap_err();
    assert!(
        matches!(&error, EvalError::HeldOutRefused { task, attempt: 0 } if task == "secret"),
        "{error}"
    );
    assert!(!args.out.exists());
}

#[test]
fn two_runs_on_the_same_input_are_byte_identical() {
    let (world, args) = failed_world();
    run_digest(&args).unwrap();
    let again = DigestArgs {
        out: world.path("out2"),
        ..args.clone()
    };
    run_digest(&again).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(&args.out)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names.len(), 3);
    for name in names {
        assert_eq!(
            std::fs::read(args.out.join(&name)).unwrap(),
            std::fs::read(again.out.join(&name)).unwrap(),
            "{name:?}"
        );
    }
}

#[test]
fn summary_json_lists_every_trial_with_its_outcome() {
    let (world, args) = failed_world();
    run_digest(&args).unwrap();
    let summary: Value = serde_json::from_str(&read(&world, "out", SUMMARY_FILE)).unwrap();
    let trials = summary["trials"].as_array().unwrap();
    assert_eq!(trials.len(), 2);
    assert_eq!(trials[0]["task"], "alpha");
    assert_eq!(trials[0]["passed"], false);
    assert_eq!(trials[0]["failure_cause"], "tests_failed");
    assert_eq!(trials[0]["stop_reason"], "stop");
    assert_eq!(trials[0]["turns"], 3);
    assert_eq!(trials[0]["digest_file"], "alpha.k0.md");
    assert_eq!(trials[0]["trace_source"], "trace.jsonl");
    assert_eq!(trials[0]["events"], 10);
    assert_eq!(trials[1]["passed"], true);
    assert_eq!(summary["candidate_cap_bytes"], 524_288);
}

#[test]
fn a_trace_that_does_not_match_the_row_hash_is_not_used() {
    let world = World::new();
    let mut failed = row("alpha", 0, false);
    world.trace("alpha__x1", &failing_trace());
    failed.trace_sha256 = Some(format!("sha256:{}", "0".repeat(64)));
    let args = world.args(&record(vec![failed]), "out");
    run_digest(&args).unwrap();
    let text = read(&world, "out", "alpha.k0.md");
    assert!(
        text.contains("- trace: unavailable (no trace.jsonl matches the row's trace_sha256"),
        "{text}"
    );
    assert!(text.contains("- failure_cause: tests_failed"));
}

#[test]
fn the_right_attempt_trace_is_chosen_by_hash() {
    let world = World::new();
    let mut a0 = row("alpha", 0, false);
    let mut a1 = row("alpha", 1, false);
    a0.trace_sha256 = Some(world.trace("alpha__a", &[call(0, "first", &json!({}))]));
    a1.trace_sha256 = Some(world.trace("alpha__b", &[call(0, "second", &json!({}))]));
    let args = world.args(&record(vec![a0, a1]), "out");
    run_digest(&args).unwrap();
    assert!(read(&world, "out", "alpha.k0.md").contains("- first: 1"));
    assert!(read(&world, "out", "alpha.k1.md").contains("- second: 1"));
}

#[test]
fn a_rerun_removes_digest_files_a_previous_run_left() {
    let (world, args) = failed_world();
    std::fs::create_dir_all(&args.out).unwrap();
    std::fs::write(args.out.join("stale.k0.md"), "old").unwrap();
    std::fs::write(args.out.join("notes.txt"), "mine").unwrap();
    run_digest(&args).unwrap();
    assert!(!args.out.join("stale.k0.md").exists());
    assert!(args.out.join("notes.txt").exists());
    drop(world);
}

#[test]
fn truncated_history_without_a_marker_is_a_compaction() {
    let world = World::new();
    let mut failed = row("alpha", 0, false);
    failed.trace_sha256 = Some(world.trace(
        "alpha__x1",
        &[
            request(0, &[("user", "a"), ("user", "b"), ("user", "c")]),
            request(1, &[("user", "a")]),
        ],
    ));
    let args = world.args(&record(vec![failed]), "out");
    run_digest(&args).unwrap();
    assert!(read(&world, "out", "alpha.k0.md").contains("- #1 history_shrank messages=3 -> 1"));
}

#[test]
fn share_budget_never_exceeds_the_budget_and_gives_small_trials_their_size() {
    let sizes = [
        (4_096, 1_000),
        (16_384, 16_000),
        (16_384, 16_000),
        (16_384, 3_000),
    ];
    let caps = share_budget(&sizes, 20_000);
    assert!(caps.iter().sum::<usize>() <= 20_000, "{caps:?}");
    assert_eq!(caps[0], 1_000);
    assert_eq!(caps[3], 3_000);
    assert!(caps[1] >= 7_000 && caps[1] == caps[2], "{caps:?}");
}

#[test]
fn share_budget_keeps_every_floor_and_still_fits_the_budget() {
    // Proportional shares of 20_000 over these weights would give the small trials ~110 bytes.
    let mut sizes = vec![(4_096, 4_096); 20];
    sizes.extend(vec![(16_384, 16_384); 20]);
    sizes.push((4_096, 100));
    let budget = 41 * MIN_TRIAL_BYTES + 600;
    let caps = share_budget(&sizes, budget);
    assert!(caps.iter().sum::<usize>() <= budget, "{caps:?}");
    for (cap, (_, size)) in caps.iter().zip(&sizes) {
        assert!(*cap >= MIN_TRIAL_BYTES.min(*size), "{caps:?}");
        assert!(cap <= size, "{caps:?}");
    }
    assert_eq!(caps[40], 100);
}

#[test]
fn a_tight_candidate_budget_redistributes_under_the_trial_floor() {
    let world = World::new();
    let big = "x".repeat(3_000);
    let names: Vec<String> = (0..540u32).map(|i| format!("task{i:03}")).collect();
    let split = json!({"seed": 1, "search": names, "held_out": ["secret"]});
    std::fs::write(world.path("split.json"), split.to_string()).unwrap();
    let mut trials = Vec::new();
    for i in 0..540u32 {
        let passed = i % 2 == 0;
        let mut r = row(&format!("task{i:03}"), 0, passed);
        let lines: Vec<Value> = (0..40)
            .map(|n| result(n, "shell", n % 7 != 3, &big, None))
            .collect();
        r.trace_sha256 = Some(world.trace(&format!("task{i:03}__0"), &lines));
        trials.push(r);
    }
    let args = world.args(&record(trials), "out");
    let outcome = run_digest(&args).unwrap();
    assert_eq!(outcome.trial_files, 540);
    let mut total = 0;
    for entry in std::fs::read_dir(&args.out).unwrap() {
        let path = entry.unwrap().path();
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        total += len;
        if path.extension().is_some_and(|e| e == "md") {
            assert!(len >= MIN_TRIAL_BYTES, "{path:?} is {len} bytes");
        }
    }
    assert!(total <= CANDIDATE_CAP, "{total} bytes");
    assert_eq!(total, outcome.total_bytes);
}

#[test]
fn digest_parses_its_arguments() {
    use clap::Parser;
    let cli = crate::bench::BenchCli::try_parse_from([
        "rsi-eval bench",
        "digest",
        "--record",
        "r.json",
        "--out",
        "d/",
    ])
    .unwrap();
    assert!(format!("{cli:?}").contains("Digest"));
}
