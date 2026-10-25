//! Tests of the bench socket client against a Rust fixture daemon on a real unix socket (design
//! #3628 "Native Harness trial and recovery"; #3682 R4). The fixture speaks the daemon's framing
//! and the typed `rsi_common::bench` shapes; no model is called and no Docker is touched. A
//! virtual pacer makes reconnect windows and agent deadlines instant.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    cell::Cell,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rsi_common::{
    bench::{BENCH_METHODS, BenchTaskStartParams, BenchTaskState},
    bench_runner::TrialExceptionKind,
};
use serde_json::{Value, json};

use super::{
    client::{
        AgentClock, BenchClient, CallError, ClientConfig, Pacer, TraceOutcome, drive_trial,
        trace_sha256, validate_socket,
    },
    trajectory::{TrajectoryInputs, build_trajectory, validate},
    trial::Clock,
};

// ---- virtual time --------------------------------------------------------------------------

struct Virtual(Cell<Duration>);

impl Virtual {
    fn at(secs: u64) -> Self {
        Self(Cell::new(Duration::from_secs(secs)))
    }
}

impl Clock for Virtual {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

impl Pacer for Virtual {
    fn sleep(&self, duration: Duration) {
        self.0.set(self.0.get() + duration);
    }
}

fn config() -> ClientConfig {
    ClientConfig {
        request_timeout: Duration::from_millis(400),
        retry_interval: Duration::from_secs(5),
        trace_page_bytes: 220,
        ..ClientConfig::default()
    }
}

// ---- the fixture socket server -------------------------------------------------------------

enum Action {
    Result(Value),
    Error {
        code: &'static str,
        retryable: bool,
    },
    /// Close the connection without a reply (the request was received and applied).
    Drop,
}

type Handler = Arc<dyn Fn(&Value) -> Action + Send + Sync>;

fn socket_tempdir() -> tempfile::TempDir {
    // Unix socket paths have a small platform limit, especially on macOS. Keep this
    // Unix-only fixture independent of a deeply nested sandbox TMPDIR.
    tempfile::Builder::new()
        .prefix("rsi-bench-client-")
        .tempdir_in("/tmp")
        .unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    raw: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(handler: Handler) -> Self {
        let dir = socket_tempdir();
        // Explicit: a default ACL or umask on the temp root must not decide the directory mode.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("adapter.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let raw = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, flag) = (Arc::clone(&raw), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let (log, handler) = (Arc::clone(&log), Arc::clone(&handler));
                        std::thread::spawn(move || serve(&mut stream, &log, &handler));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            _dir: dir,
            path,
            raw,
            stop,
            thread: Some(thread),
        }
    }

    fn client(&self) -> BenchClient {
        BenchClient::open(&self.path, config()).unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        self.raw
            .lock()
            .unwrap()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r["method"].as_str().unwrap().to_owned())
            .collect()
    }

    fn count(&self, method: &str) -> usize {
        self.methods().iter().filter(|m| *m == method).count()
    }

    /// Stop accepting: later connects are refused.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn serve(stream: &mut std::os::unix::net::UnixStream, log: &Mutex<Vec<String>>, handler: &Handler) {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];
    while stream.read(&mut byte).unwrap_or(0) == 1 {
        if byte[0] == b'\n' {
            break;
        }
        buffer.push(byte[0]);
    }
    let line = String::from_utf8(buffer).unwrap();
    log.lock().unwrap().push(line.clone());
    let request: Value = serde_json::from_str(&line).unwrap();
    let reply = match handler(&request) {
        Action::Drop => return,
        Action::Result(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
        Action::Error { code, retryable } => json!({
            "jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32602, "message": format!("{code}: refused"),
                      "data": {"code": code, "retryable": retryable}}
        }),
    };
    let mut text = reply.to_string();
    text.push('\n');
    let _ = stream.write_all(text.as_bytes());
}

// ---- the fake daemon -----------------------------------------------------------------------

#[derive(Clone)]
struct Knobs {
    /// `BenchTaskStatus` answers `running` this many times, then the final state.
    running_polls: u32,
    /// The session never ends on its own; only Cancel settles it.
    never_ends: bool,
    final_state: &'static str,
    stop_reason: Option<&'static str>,
    route_verdict: Option<&'static str>,
    /// `BenchTaskFinish` answers `bench_task_not_finished` this many times (u32::MAX: forever).
    finish_busy: u32,
    /// The first N `BenchTaskStart` replies are lost after the start was applied.
    drop_start_replies: u32,
    drop_status: bool,
    drop_cancel: bool,
    start_refusal: Option<&'static str>,
    trace: String,
    tamper_trace_sha: bool,
    /// A status whose `state` is not one the protocol defines.
    mystery_state: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            running_polls: 2,
            never_ends: false,
            final_state: "finished",
            stop_reason: Some("end_turn"),
            route_verdict: Some("valid"),
            finish_busy: 0,
            drop_start_replies: 0,
            drop_status: false,
            drop_cancel: false,
            start_refusal: None,
            trace: trace_text(),
            tamper_trace_sha: false,
            mystery_state: false,
        }
    }
}

#[derive(Default)]
struct Inner {
    started: Option<Value>,
    status_calls: u32,
    cancelled: bool,
    start_calls: u32,
    finish_calls: u32,
}

fn page(trace: &str, offset: u64, limit: u64) -> Value {
    let bytes = trace.as_bytes();
    let offset = usize::try_from(offset).unwrap();
    let mut window = usize::try_from(limit).unwrap();
    loop {
        let end = (offset + window).min(bytes.len());
        let slice = &bytes[offset..end];
        if let Some(at) = slice.iter().rposition(|b| *b == b'\n') {
            let next = offset + at + 1;
            return json!({"offset": offset, "next_offset": next,
                          "eof": next >= bytes.len(),
                          "data": String::from_utf8(bytes[offset..next].to_vec()).unwrap()});
        }
        if end >= bytes.len() {
            return json!({"offset": offset, "next_offset": offset, "eof": true, "data": ""});
        }
        window *= 2;
    }
}

fn daemon(knobs: Knobs) -> Handler {
    let inner = Mutex::new(Inner::default());
    Arc::new(move |request: &Value| {
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        let mut state = inner.lock().unwrap();
        let live = |s: &Inner| {
            if s.cancelled {
                return false;
            }
            knobs.never_ends || s.status_calls <= knobs.running_polls
        };
        let status = |s: &Inner, settled_trace: bool| {
            let trial = s
                .started
                .as_ref()
                .map_or_else(|| params["trial"].clone(), |p| p["trial"].clone());
            let (state_name, stop) = if s.cancelled {
                ("cancelled", Some("bench_cancelled"))
            } else if live(s) {
                ("running", None)
            } else {
                (knobs.final_state, knobs.stop_reason)
            };
            let sha = if settled_trace && state_name != "running" {
                let mut sha = trace_sha256(knobs.trace.as_bytes());
                if knobs.tamper_trace_sha {
                    sha.replace_range(0..4, "0000");
                }
                json!(sha)
            } else {
                Value::Null
            };
            let wire_state = if knobs.mystery_state {
                "mystery"
            } else {
                state_name
            };
            let route = if state_name == "running" {
                Value::Null
            } else {
                json!(knobs.route_verdict)
            };
            let violations = if knobs.route_verdict == Some("invalid_route") {
                json!(["model off the allowlist"])
            } else {
                json!([])
            };
            json!({
                "run_id": "run-1", "task": "fix-git", "trial": trial, "attempt": 1,
                "state": wire_state,
                "stop_reason": stop,
                "session_id": "sess-1",
                "usage": {"cost_usd": 0.0123, "prompt_tokens": 1200, "cached_tokens": 900,
                          "output_tokens": 300, "reasoning_tokens": 0, "wall_ms": 4000, "calls": 2},
                "route_verdict": route,
                "route_violations": violations,
                "harness_build_digest": "sha256:build",
                "trace": {"path": "/x/trace.jsonl", "bytes": knobs.trace.len(), "sha256": sha},
            })
        };
        match method {
            "BenchTaskStart" => {
                if let Some(code) = knobs.start_refusal {
                    return Action::Error {
                        code,
                        retryable: false,
                    };
                }
                state.start_calls += 1;
                let prior = state.started.clone();
                match prior {
                    Some(prior) if prior != *params => {
                        return Action::Error {
                            code: "bench_trial_conflict",
                            retryable: false,
                        };
                    }
                    Some(_) => {}
                    None => state.started = Some(params.clone()),
                }
                if state.start_calls <= knobs.drop_start_replies {
                    return Action::Drop;
                }
                Action::Result(status(&state, false))
            }
            "BenchTaskStatus" => {
                if knobs.drop_status {
                    return Action::Drop;
                }
                state.status_calls += 1;
                Action::Result(status(&state, false))
            }
            "BenchTaskCancel" => {
                if knobs.drop_cancel {
                    return Action::Drop;
                }
                state.cancelled = true;
                Action::Result(status(&state, false))
            }
            "BenchTaskFinish" => {
                state.finish_calls += 1;
                if live(&state) || state.finish_calls <= knobs.finish_busy {
                    return Action::Error {
                        code: "bench_task_not_finished",
                        retryable: true,
                    };
                }
                Action::Result(status(&state, true))
            }
            "BenchTraceRead" => {
                let offset = params["offset"].as_u64().unwrap_or(0);
                let limit = params["limit_bytes"].as_u64().unwrap_or(256 * 1024);
                Action::Result(page(&knobs.trace, offset, limit))
            }
            _ => Action::Error {
                code: "bench_method_not_allowed",
                retryable: false,
            },
        }
    })
}

fn trace_text() -> String {
    let lines = [
        json!({"seq": 0, "kind": "model_request", "request": {"model": "vendor/model-a", "messages": []}}),
        json!({"seq": 1, "kind": "model_response", "response": {
            "content": "Looking.", "reasoning_content": "think",
            "tool_calls": [{"id": "c1", "name": "shell", "arguments": "{\"command\":\"ls\"}"}],
            "stop_reason": "tool_calls",
            "usage": {"prompt_tokens": 700, "completion_tokens": 20, "total_tokens": 720,
                      "cache_creation_tokens": 0, "cache_read_tokens": 500,
                      "reasoning_tokens": 5, "cost_usd": 0.001}}}),
        json!({"seq": 2, "kind": "tool_call", "name": "shell", "args": {"command": "ls"}}),
        json!({"seq": 3, "kind": "tool_result", "name": "shell",
               "result": {"success": true, "output": "a.txt\n", "error_msg": null}}),
        json!({"seq": 4, "kind": "model_request", "request": {"model": "vendor/model-a", "messages": [1, 2, 3]}}),
        json!({"seq": 5, "kind": "model_response", "response": {
            "content": "Done.", "reasoning_content": null, "tool_calls": [], "stop_reason": "stop",
            "usage": {"prompt_tokens": 500, "completion_tokens": 10, "total_tokens": 510,
                      "cache_creation_tokens": 0, "cache_read_tokens": 0,
                      "reasoning_tokens": 0, "cost_usd": null}}}),
    ];
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn start_params() -> BenchTaskStartParams {
    BenchTaskStartParams {
        task: "fix-git".into(),
        trial: "fix-git__r1".into(),
        compose_project: "fix-git__r1".into(),
        container: "fix-git__r1-main-1".into(),
        workdir: "/app".into(),
        user: None,
        instruction: "Fix the repo.".into(),
        agent_timeout_sec: 100,
    }
}

fn kinds(result: &super::client::HarnessTrialResult) -> Vec<TrialExceptionKind> {
    result.exceptions.iter().map(|e| e.kind).collect()
}

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn all_five_methods_are_spoken_and_no_token_is_sent() {
    let fixture = Fixture::start(daemon(Knobs {
        running_polls: 0,
        ..Knobs::default()
    }));
    let client = fixture.client();
    let pacer = Virtual::at(0);
    let params = start_params();
    let started = client.start(&pacer, &params, None).unwrap();
    assert_eq!(started.trial, params.trial);
    assert_eq!(started.state, BenchTaskState::Running);
    assert_eq!(
        client.status(&pacer, &params.trial, None).unwrap().trial,
        params.trial
    );
    let cancelled = client
        .cancel(&pacer, &params.trial, Duration::from_secs(10))
        .unwrap();
    assert_eq!(cancelled.state, BenchTaskState::Cancelled);
    let finished = client.finish(&pacer, &params.trial, None).unwrap();
    let handle = finished.trace.unwrap();
    let trace = client.read_trace(&pacer, &params.trial, &handle).unwrap();
    assert_eq!(trace, trace_text());

    let mut seen = fixture.methods();
    seen.sort();
    seen.dedup();
    let mut expected: Vec<String> = BENCH_METHODS.iter().map(|m| (*m).to_owned()).collect();
    expected.sort();
    assert_eq!(seen, expected, "exactly the five Bench methods");
    // The full trace came back through several pages.
    assert!(fixture.count("BenchTraceRead") > 1);
    // Newline JSON-RPC framing; no token sibling, no run id, nothing but the four members.
    for raw in fixture.raw.lock().unwrap().iter() {
        let value: Value = serde_json::from_str(raw).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["id", "jsonrpc", "method", "params"]);
        assert!(!raw.to_ascii_lowercase().contains("token"), "{raw}");
        assert!(value["params"].get("run").is_none() && value["params"].get("run_id").is_none());
    }
}

#[test]
fn a_full_trial_settles_then_reads_the_whole_trace_and_builds_atif() {
    let fixture = Fixture::start(daemon(Knobs::default()));
    let pacer = Virtual::at(0);
    let result = drive_trial(&fixture.client(), &pacer, &start_params(), None);
    assert!(result.verify, "{:?}", result.exceptions);
    assert!(result.exceptions.is_empty());
    assert!(!result.cancel_requested);
    let status = result.status.clone().unwrap();
    assert_eq!(status.usage.prompt_tokens, 1200);
    assert!(
        matches!(&result.trace, TraceOutcome::Complete { jsonl, .. } if *jsonl == trace_text())
    );
    // Finish before the trace is paged.
    let methods = fixture.methods();
    let finish_at = methods.iter().position(|m| m == "BenchTaskFinish").unwrap();
    let first_page = methods.iter().position(|m| m == "BenchTraceRead").unwrap();
    assert!(finish_at < first_page);
    assert_eq!(fixture.count("BenchTaskStart"), 1);

    let trajectory = build_trajectory(
        &TrajectoryInputs {
            instruction: "Fix the repo.",
            status: &status,
            agent_version: "test",
        },
        &result.trace,
    );
    validate(&trajectory).unwrap();
    assert_eq!(trajectory.steps.len(), 3);
    assert_eq!(trajectory.final_metrics.total_prompt_tokens, Some(1200));
    assert_eq!(trajectory.final_metrics.total_cost_usd, Some(0.0123));
}

#[test]
fn a_lost_start_reply_is_retried_with_the_identical_request() {
    let fixture = Fixture::start(daemon(Knobs {
        drop_start_replies: 2,
        ..Knobs::default()
    }));
    let pacer = Virtual::at(0);
    let result = drive_trial(&fixture.client(), &pacer, &start_params(), None);
    assert!(result.verify, "{:?}", result.exceptions);
    let starts: Vec<Value> = fixture
        .requests()
        .into_iter()
        .filter(|r| r["method"] == "BenchTaskStart")
        .collect();
    assert_eq!(starts.len(), 3, "two lost replies and the answered one");
    assert!(
        starts
            .iter()
            .all(|s| s["params"] == starts[0]["params"] && s["id"] == starts[0]["id"])
    );
    // The reply loss cost agent time on the original clock, not a new trial.
    assert!(result.agent_elapsed_ms >= 10_000);
}

#[test]
fn an_unreachable_daemon_is_infra_and_never_a_grade() {
    let mut fixture = Fixture::start(daemon(Knobs::default()));
    let client = fixture.client();
    fixture.shutdown();
    let pacer = Virtual::at(0);
    let result = drive_trial(&client, &pacer, &start_params(), None);
    assert!(!result.verify);
    assert_eq!(kinds(&result), [TrialExceptionKind::InfraInterrupted]);
    assert!(
        pacer.now() >= Duration::from_secs(60),
        "the reconnect window was spent"
    );
    assert!(
        pacer.now() < Duration::from_secs(100),
        "and never the agent clock"
    );
}

#[test]
fn a_status_link_lost_before_the_deadline_cancels_and_is_infra() {
    let fixture = Fixture::start(daemon(Knobs {
        drop_status: true,
        drop_cancel: true,
        never_ends: true,
        ..Knobs::default()
    }));
    let pacer = Virtual::at(0);
    let result = drive_trial(&fixture.client(), &pacer, &start_params(), None);
    assert!(!result.verify);
    assert!(result.cancel_requested);
    assert!(fixture.count("BenchTaskCancel") > 0);
    assert!(
        kinds(&result)
            .iter()
            .all(|k| *k == TrialExceptionKind::InfraInterrupted)
    );
}

#[test]
fn a_timeout_cancels_and_grades_only_after_settlement() {
    let fixture = Fixture::start(daemon(Knobs {
        never_ends: true,
        finish_busy: 2,
        ..Knobs::default()
    }));
    let pacer = Virtual::at(0);
    let result = drive_trial(&fixture.client(), &pacer, &start_params(), None);
    assert!(
        result.verify,
        "settled after cancel, so it may be graded: {:?}",
        result.exceptions
    );
    assert_eq!(kinds(&result), [TrialExceptionKind::AgentTimeout]);
    assert!(result.cancel_requested);
    assert!(
        result.agent_elapsed_ms >= 100_000,
        "{}",
        result.agent_elapsed_ms
    );
    assert!(
        result.agent_elapsed_ms < 110_000,
        "polling never overshoots the clock"
    );
    let methods = fixture.methods();
    let cancel = methods.iter().position(|m| m == "BenchTaskCancel").unwrap();
    let first_finish = methods.iter().position(|m| m == "BenchTaskFinish").unwrap();
    assert!(cancel < first_finish, "Cancel before Finish");
    assert_eq!(
        fixture.count("BenchTaskFinish"),
        3,
        "two busy answers, then settled"
    );
    let last_finish = methods
        .iter()
        .rposition(|m| m == "BenchTaskFinish")
        .unwrap();
    assert!(methods.iter().position(|m| m == "BenchTraceRead").unwrap() > last_finish);
    assert_eq!(result.status.unwrap().state, BenchTaskState::Cancelled);
}

#[test]
fn a_session_that_cannot_be_settled_is_not_verified() {
    let busy = Fixture::start(daemon(Knobs {
        never_ends: true,
        finish_busy: u32::MAX,
        ..Knobs::default()
    }));
    let result = drive_trial(&busy.client(), &Virtual::at(0), &start_params(), None);
    assert!(!result.verify);
    assert_eq!(
        kinds(&result),
        [
            TrialExceptionKind::AgentTimeout,
            TrialExceptionKind::AgentSettleFailed
        ]
    );
    assert_eq!(busy.count("BenchTraceRead"), 0);

    let uncancellable = Fixture::start(daemon(Knobs {
        never_ends: true,
        drop_cancel: true,
        ..Knobs::default()
    }));
    let result = drive_trial(
        &uncancellable.client(),
        &Virtual::at(0),
        &start_params(),
        None,
    );
    assert!(!result.verify);
    assert_eq!(
        kinds(&result),
        [
            TrialExceptionKind::AgentTimeout,
            TrialExceptionKind::AgentSettleFailed
        ]
    );
    assert_eq!(uncancellable.count("BenchTaskFinish"), 0);
}

#[test]
fn the_original_agent_clock_is_honored_on_resume() {
    let fixture = Fixture::start(daemon(Knobs {
        never_ends: true,
        ..Knobs::default()
    }));
    let pacer = Virtual::at(300);
    let clock = AgentClock {
        started: Duration::from_secs(100),
        timeout: Duration::from_secs(100),
    };
    let result = drive_trial(&fixture.client(), &pacer, &start_params(), Some(clock));
    assert_eq!(kinds(&result), [TrialExceptionKind::AgentTimeout]);
    assert!(result.verify);
    assert!(
        result.agent_elapsed_ms >= 200_000,
        "measured from the original start"
    );
    assert_eq!(
        fixture.count("BenchTaskStatus"),
        0,
        "no polling past a spent deadline"
    );
    assert_eq!(fixture.count("BenchTaskStart"), 0);
    assert_eq!(fixture.count("BenchTaskCancel"), 1);
    assert!(result.cancel_requested);
}

#[test]
fn an_unknown_state_is_invalid_and_cancels() {
    let fixture = Fixture::start(daemon(Knobs {
        mystery_state: true,
        ..Knobs::default()
    }));
    let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
    assert!(!result.verify);
    assert_eq!(
        kinds(&result).last(),
        Some(&TrialExceptionKind::InvalidPlan),
        "the cause is last"
    );
    assert!(fixture.count("BenchTaskCancel") > 0);
}

#[test]
fn refusals_are_typed_and_start_nothing() {
    for (code, kind) in [
        (
            "bench_launch_not_allowed",
            TrialExceptionKind::RouteRejected,
        ),
        (
            "bench_run_budget_insufficient",
            TrialExceptionKind::SpendCapRejected,
        ),
        (
            "openrouter_spend_cap_exhausted",
            TrialExceptionKind::SpendCapRejected,
        ),
        (
            "bench_concurrency_exceeded",
            TrialExceptionKind::InvalidPlan,
        ),
        ("bench_run_not_active", TrialExceptionKind::InvalidPlan),
    ] {
        let fixture = Fixture::start(daemon(Knobs {
            start_refusal: Some(code),
            ..Knobs::default()
        }));
        let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
        assert!(!result.verify, "{code}");
        assert_eq!(kinds(&result), [kind], "{code}");
        assert!(
            !result.cancel_requested,
            "{code}: a refused Start created no session"
        );
        assert_eq!(
            fixture.count("BenchTaskStart"),
            1,
            "{code}: refusals are not retried"
        );
    }
}

#[test]
fn an_invalid_route_keeps_usage_and_trace_but_is_not_graded() {
    let fixture = Fixture::start(daemon(Knobs {
        route_verdict: Some("invalid_route"),
        ..Knobs::default()
    }));
    let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
    assert!(!result.verify);
    assert_eq!(kinds(&result), [TrialExceptionKind::RouteRejected]);
    assert_eq!(result.status.unwrap().usage.prompt_tokens, 1200);
    assert!(matches!(result.trace, TraceOutcome::Complete { .. }));
}

#[test]
fn a_daemon_restart_state_is_infra_with_usage_and_trace_retained() {
    let fixture = Fixture::start(daemon(Knobs {
        final_state: "infra_interrupted",
        stop_reason: Some("infra_interrupted"),
        ..Knobs::default()
    }));
    let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
    assert!(!result.verify);
    assert_eq!(kinds(&result), [TrialExceptionKind::InfraInterrupted]);
    assert_eq!(result.status.unwrap().usage.cost_usd, Some(0.0123));
    assert!(matches!(result.trace, TraceOutcome::Complete { .. }));
}

#[test]
fn the_daemons_own_timeout_stop_is_graded_as_an_agent_timeout() {
    let fixture = Fixture::start(daemon(Knobs {
        final_state: "cancelled",
        stop_reason: Some("bench_agent_timeout"),
        ..Knobs::default()
    }));
    let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
    assert!(result.verify);
    assert_eq!(kinds(&result), [TrialExceptionKind::AgentTimeout]);
    assert!(!result.cancel_requested);
}

#[test]
fn a_per_task_budget_stop_is_a_normal_completion() {
    let fixture = Fixture::start(daemon(Knobs {
        stop_reason: Some("bench_task_budget_exhausted"),
        ..Knobs::default()
    }));
    let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
    assert!(result.verify);
    assert!(result.exceptions.is_empty());
}

#[test]
fn a_trace_that_does_not_match_its_digest_is_unavailable_never_complete() {
    let fixture = Fixture::start(daemon(Knobs {
        tamper_trace_sha: true,
        ..Knobs::default()
    }));
    let result = drive_trial(&fixture.client(), &Virtual::at(0), &start_params(), None);
    assert!(result.verify, "the trace never changes the grade");
    let TraceOutcome::Unavailable { reason } = &result.trace else {
        panic!("a tampered trace must not be reported complete");
    };
    assert!(reason.contains("digest"), "{reason}");
    let status = result.status.unwrap();
    let trajectory = build_trajectory(
        &TrajectoryInputs {
            instruction: "Fix the repo.",
            status: &status,
            agent_version: "test",
        },
        &result.trace,
    );
    assert_eq!(trajectory.steps.len(), 1);
    assert!(trajectory.notes.unwrap().starts_with("trace unavailable"));
}

#[test]
fn malformed_start_params_never_reach_the_socket() {
    let fixture = Fixture::start(daemon(Knobs::default()));
    let mut params = start_params();
    params.instruction = "   ".into();
    let error = fixture
        .client()
        .start(&Virtual::at(0), &params, None)
        .unwrap_err();
    assert!(
        matches!(&error, CallError::Protocol(m) if m.contains("instruction")),
        "{error:?}"
    );
    assert!(fixture.requests().is_empty());
}

#[test]
fn a_reply_for_another_request_is_a_protocol_error() {
    let fixture = Fixture::start(Arc::new(|_: &Value| {
        Action::Result(json!({"trial": "other"}))
    }));
    let error = fixture
        .client()
        .status(&Virtual::at(0), "fix-git__r1", None)
        .unwrap_err();
    assert!(matches!(error, CallError::Protocol(_)), "{error:?}");
}

#[test]
fn the_socket_is_checked_before_any_connection() {
    let dir = socket_tempdir();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let missing = dir.path().join("adapter.sock");
    assert!(matches!(
        BenchClient::open(&missing, config()),
        Err(CallError::Unsupported(_))
    ));
    assert!(matches!(
        BenchClient::open(Path::new("adapter.sock"), config()),
        Err(CallError::Unsupported(_))
    ));

    // A regular file is not a socket.
    std::fs::write(&missing, "x").unwrap();
    assert!(validate_socket(&missing).is_err());
    std::fs::remove_file(&missing).unwrap();

    // A socket under the daemon's general name is refused: no fallback to `daemon.sock`.
    let daemon_sock = dir.path().join("daemon.sock");
    let _listener = UnixListener::bind(&daemon_sock).unwrap();
    std::fs::set_permissions(&daemon_sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = validate_socket(&daemon_sock).unwrap_err();
    assert!(matches!(&error, CallError::Unsupported(m) if m.contains("adapter.sock")));

    // A bound adapter socket passes only when closed to group and others.
    let adapter = dir.path().join("adapter.sock");
    let _bound = UnixListener::bind(&adapter).unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(validate_socket(&adapter).is_err());
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o600)).unwrap();
    validate_socket(&adapter).unwrap();
    // …and when its directory is private.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(validate_socket(&adapter).is_err());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

// ---- #3843: one attempt never outlives the caller's cap -------------------------------------

#[test]
fn an_expired_call_never_sends_a_request() {
    let fixture = Fixture::start(daemon(Knobs::default()));
    let client = fixture.client();
    let pacer = Virtual::at(100);
    for cap in [Duration::from_secs(99), Duration::from_secs(100)] {
        assert!(matches!(
            client.status(&pacer, "fix-git__t", Some(cap)),
            Err(CallError::Unreachable(_))
        ));
    }
    assert!(fixture.requests().is_empty());
}

#[test]
fn a_reply_inside_the_old_attempt_floor_but_after_the_cap_is_rejected() {
    let handler = daemon(Knobs::default());
    let fixture = Fixture::start(Arc::new(move |request| {
        std::thread::sleep(Duration::from_millis(25));
        handler(request)
    }));
    let pacer = Virtual::at(100);
    let result =
        fixture
            .client()
            .status(&pacer, "fix-git__t", Some(Duration::from_millis(100_005)));
    assert!(matches!(result, Err(CallError::Lost(_))), "{result:?}");
}

#[test]
fn an_attempt_waits_no_longer_than_the_callers_cap_even_with_a_long_request_timeout() {
    // The server holds the request for 3 s and never answers. The client's request timeout is
    // 30 s, but the caller's cap is 0.3 s away: the call must give up at the cap, not at 30 s.
    let fixture = Fixture::start(Arc::new(|_: &Value| {
        std::thread::sleep(Duration::from_secs(3));
        Action::Drop
    }));
    let client = BenchClient::open(
        &fixture.path,
        ClientConfig {
            request_timeout: Duration::from_secs(30),
            ..config()
        },
    )
    .unwrap();
    let pacer = Virtual::at(100);
    let started = std::time::Instant::now();
    let error = client
        .status(&pacer, "fix-git__t", Some(Duration::from_millis(100_300)))
        .unwrap_err();
    let waited = started.elapsed();
    assert!(
        matches!(error, CallError::Lost(_)),
        "the request may have been delivered: {error:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "the attempt ran {waited:?} past a 0.3 s cap"
    );
}
