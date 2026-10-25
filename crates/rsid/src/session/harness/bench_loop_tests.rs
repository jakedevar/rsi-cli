//! #3178: the bench hooks of the Harness loop, driven through the real loop with
//! a fake provider: the live trace matches the provider transcript exactly, and
//! the per-task USD cap ends a session before the call that would cross it.

use super::agent_loop::run_harness_loop_with_compact_budget;
use super::agent_loop::tests::{controller, root_permit};
use super::tools::{HarnessTool, HarnessToolRegistry};
use super::types::{
    ChatMessage, ChatRequest, ChatResponse, StreamChunk, TokenUsage, ToolCall, ToolResult,
};
use crate::bench::cap::BenchTaskGuardedControl;
use crate::bench::trace::TraceSink;
use crate::error::DaemonError;
use crate::session::harness::provider::ApiProvider;
use crate::store::Store;
use crate::store::bench::{BenchRunTask, NewBenchRun};
use rsi_common::bench::{
    BENCH_TASK_BUDGET_EXHAUSTED, BENCH_TASK_COST_UNKNOWN, BENCH_TASK_NOT_RUNNING,
    BENCH_TRACE_WRITE_FAILED,
};
use rsi_common::bench_profile::LoopKnobs;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A provider that replays scripted responses and keeps every request it saw.
struct ScriptedProvider {
    script: Mutex<VecDeque<ChatResponse>>,
    /// A response repeated once the script runs out (a model that never stops).
    forever: Option<ChatResponse>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait::async_trait]
impl ApiProvider for ScriptedProvider {
    async fn chat(
        &self,
        request: &ChatRequest,
        _execution: crate::model_control::ModelExecutionCapability,
    ) -> crate::error::Result<ChatResponse> {
        self.seen.lock().await.push(request.clone());
        if let Some(next) = self.script.lock().await.pop_front() {
            return Ok(next);
        }
        self.forever
            .clone()
            .ok_or_else(|| DaemonError::Process("the script ran out".into()))
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        _chunks: mpsc::Sender<StreamChunk>,
        _cancel: &CancellationToken,
        execution: crate::model_control::ModelExecutionCapability,
    ) -> crate::error::Result<ChatResponse> {
        self.chat(request, execution).await
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "scripted"
    }

    fn supports_streaming(&self) -> bool {
        false
    }
}

struct EchoTool;

#[async_trait::async_trait]
impl HarnessTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the text back."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["text"],"properties":{"text":{"type":"string"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        // Longer than the 2,000-char agent-read clip, shorter than the loop's
        // own spill threshold.
        let text = args["text"].as_str().unwrap_or_default();
        ToolResult {
            success: true,
            output: format!("echo: {text} {}", "z".repeat(5_000)),
            error_msg: None,
        }
    }
}

fn tool_call_response(id: &str, cost: f64) -> ChatResponse {
    ChatResponse {
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "echo".into(),
            arguments: r#"{"text":"hi"}"#.into(),
            hosted: false,
            hosted_result: None,
        }],
        usage: usage(cost),
        reasoning_content: None,
        reasoning_details: None,
        stop_reason: Some("tool_calls".into()),
    }
}

fn usage(cost: f64) -> TokenUsage {
    TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 10,
        total_tokens: 110,
        cost_usd: Some(cost),
        ..TokenUsage::default()
    }
}

fn final_response(text: &str, cost: f64) -> ChatResponse {
    ChatResponse {
        content: text.into(),
        tool_calls: Vec::new(),
        usage: usage(cost),
        reasoning_content: None,
        reasoning_details: None,
        stop_reason: Some("stop".into()),
    }
}

async fn run(
    provider: ScriptedProvider,
    tools: HarnessToolRegistry,
    control: Arc<dyn crate::model_control::call_control::ModelCallControl>,
) -> Result<(), DaemonError> {
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let drain = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let result = run_harness_loop_with_compact_budget(
        Box::new(provider),
        Arc::new(tools),
        String::new(),
        "do the task".to_string(),
        std::env::temp_dir(),
        "gpt-5.4".to_string(),
        None,
        event_tx,
        CancellationToken::new(),
        control,
        25,
        100_000,
        None,
        None,
        Default::default(),
        None,
        None,
        true,
    )
    .await;
    let _ = drain.await;
    result
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_trace_matches_the_provider_transcript_exactly() {
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let session_id = Uuid::new_v4();
    let permit = root_permit(&store, session_id).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t").join("trace.jsonl");
    let sink = Arc::new(TraceSink::open(&path).unwrap());

    let first = tool_call_response("call-1", 0.01);
    let last = final_response("all done", 0.02);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        script: Mutex::new(VecDeque::from([first.clone(), last.clone()])),
        forever: None,
        seen: Arc::clone(&seen),
    };
    let mut tools = HarnessToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools.set_trace(Arc::clone(&sink));
    run(provider, tools, controller(&store, session_id, permit))
        .await
        .expect("the loop ends");

    let requests = seen.lock().await.clone();
    assert_eq!(requests.len(), 2);
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<_> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "model_request",
            "model_response",
            "tool_call",
            "tool_result",
            "model_request",
            "model_response"
        ]
    );
    for (n, line) in lines.iter().enumerate() {
        assert_eq!(line["seq"], n);
    }
    // Each request is exactly what the provider was handed.
    for (line, request) in [&lines[0], &lines[4]].into_iter().zip(&requests) {
        assert_eq!(
            line["request"]["messages"],
            serde_json::to_value(&request.messages).unwrap()
        );
        assert_eq!(line["request"]["model"], request.model);
        assert_eq!(
            line["request"]["tools"],
            serde_json::to_value(&request.tools).unwrap()
        );
    }
    // Each response is exactly what the provider returned.
    assert_eq!(lines[1]["response"]["content"], first.content);
    assert_eq!(
        lines[1]["response"]["tool_calls"],
        serde_json::to_value(&first.tool_calls).unwrap()
    );
    assert_eq!(lines[1]["response"]["usage"]["cost_usd"], 0.01);
    assert_eq!(lines[5]["response"]["content"], "all done");
    assert_eq!(lines[5]["response"]["usage"]["completion_tokens"], 10);
    // The tool call and the result the model saw, whole (over 5,000 bytes).
    assert_eq!(lines[2]["name"], "echo");
    assert_eq!(lines[2]["args"], serde_json::json!({"text": "hi"}));
    assert_eq!(lines[3]["name"], "echo");
    let output = lines[3]["result"]["output"].as_str().unwrap();
    assert!(output.starts_with("echo: hi z"));
    assert!(output.len() > 5_000, "not clipped: {}", output.len());
    // The second request carries that very result back to the model.
    let tool_message: &ChatMessage = requests[1]
        .messages
        .iter()
        .rev()
        .find(|m| m.tool_call_id.as_deref() == Some("call-1"))
        .expect("the tool result is in the next request");
    assert_eq!(tool_message.content, output);
    assert!(sink.healthy());
}

/// #3349: the failed append is the final response of a completed turn, so no
/// later admission checks trace health. The call that carried it still ends
/// the session with the typed code (after its usage is recorded), never a
/// normal completion.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_failed_final_trace_append_does_not_end_the_session_as_completed() {
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let session_id = Uuid::new_v4();
    let permit = root_permit(&store, session_id).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t").join("trace.jsonl");
    let sink = Arc::new(TraceSink::open(&path).unwrap());
    // Line 0 is the request; line 1, the final response, cannot be written.
    sink.fail_writes_from_for_test(1);

    let provider = ScriptedProvider {
        script: Mutex::new(VecDeque::from([final_response("all done", 0.02)])),
        forever: None,
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let mut tools = HarnessToolRegistry::new();
    tools.set_trace(Arc::clone(&sink));
    let error = run(provider, tools, controller(&store, session_id, permit))
        .await
        .expect_err("a trace with a gap does not complete");
    assert!(
        matches!(&error, DaemonError::PolicyDenied(text) if text.starts_with(BENCH_TRACE_WRITE_FAILED)),
        "{error:?}"
    );
    assert!(!sink.healthy());
    assert!(crate::bench::trace::path_write_failed(&path));
    // The call's own usage was still recorded before the session ended.
    let usage = store.lock().await.bench_session_usage(session_id).unwrap();
    assert_eq!(usage.calls, 1);
}

fn bench_run(id: Uuid, cap: f64) -> NewBenchRun {
    NewBenchRun {
        id,
        project_id: Uuid::new_v4(),
        job_id: None,
        config_path: "eval/bench/tb2/runs/x.toml".into(),
        config_digest: format!("sha256:{}", "c".repeat(64)),
        dataset_commit: "2fd12b88aafdd04a52c298e3940bcb189f9766d6".into(),
        docker_host: "ssh://arch-laptop".into(),
        model: "deepseek/deepseek-v4.1-flash".into(),
        effort: "high".into(),
        budget_usd: 10.0,
        per_task_cap_usd: cap,
        concurrency: 1,
        tasks: vec![BenchRunTask {
            task: "fix-git".into(),
            pinned_digests: vec![format!("sha256:{}", "a".repeat(64))],
        }],
        loop_knobs: LoopKnobs::default(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_per_task_cap_ends_the_session_before_the_call_that_would_cross_it() {
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let session_id = Uuid::new_v4();
    let permit = root_permit(&store, session_id).await;
    // A model that calls a tool, at $0.15 a call, forever; the cap is $0.40.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        script: Mutex::new(VecDeque::new()),
        forever: Some(tool_call_response("loop", 0.15)),
        seen: Arc::clone(&seen),
    };
    let mut tools = HarnessToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let guarded: Arc<dyn crate::model_control::call_control::ModelCallControl> =
        Arc::new(BenchTaskGuardedControl::new(
            controller(&store, session_id, permit),
            Arc::clone(&store),
            Some(session_id),
            Some(400_000),
            Some(0.0),
            None,
        ));
    let error = run(provider, tools, guarded)
        .await
        .expect_err("the cap ends the session");
    assert!(
        matches!(&error, DaemonError::PolicyDenied(text) if text.starts_with(BENCH_TASK_BUDGET_EXHAUSTED)),
        "{error:?}"
    );
    // Two calls ($0.30) were allowed; the third could have crossed $0.40.
    assert_eq!(seen.lock().await.len(), 2);
    let usage = store.lock().await.bench_session_usage(session_id).unwrap();
    assert_eq!(usage.calls, 2);
    let spent = usage.cost_usd.unwrap();
    assert!((spent - 0.30).abs() < 1e-9 && spent < 0.40, "spent {spent}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_call_bound_keeps_every_admitted_call_under_the_cap() {
    // Calls cost $0.15; one call may cost up to $0.20 of a $0.40 cap (#3350).
    // The history-only reserve would admit a third call at $0.30 spent and end
    // at $0.45; the bound refuses it.
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let session_id = Uuid::new_v4();
    let permit = root_permit(&store, session_id).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        script: Mutex::new(VecDeque::new()),
        forever: Some(tool_call_response("loop", 0.15)),
        seen: Arc::clone(&seen),
    };
    let mut tools = HarnessToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let guarded: Arc<dyn crate::model_control::call_control::ModelCallControl> =
        Arc::new(BenchTaskGuardedControl::new(
            controller(&store, session_id, permit),
            Arc::clone(&store),
            Some(session_id),
            Some(400_000),
            Some(0.20),
            None,
        ));
    let error = run(provider, tools, guarded)
        .await
        .expect_err("the cap ends the session");
    assert!(
        matches!(&error, DaemonError::PolicyDenied(text) if text.starts_with(BENCH_TASK_BUDGET_EXHAUSTED)),
        "{error:?}"
    );
    // Two calls ($0.30): the third's bound would reach $0.50.
    assert_eq!(seen.lock().await.len(), 2);

    // A bound as large as the cap refuses even the first call: no model call.
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let session_id = Uuid::new_v4();
    let permit = root_permit(&store, session_id).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        script: Mutex::new(VecDeque::new()),
        forever: Some(tool_call_response("loop", 0.15)),
        seen: Arc::clone(&seen),
    };
    let mut tools = HarnessToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let guarded: Arc<dyn crate::model_control::call_control::ModelCallControl> =
        Arc::new(BenchTaskGuardedControl::new(
            controller(&store, session_id, permit),
            Arc::clone(&store),
            Some(session_id),
            Some(400_000),
            Some(0.50),
            None,
        ));
    let error = run(provider, tools, guarded)
        .await
        .expect_err("the first call is refused");
    assert!(
        matches!(&error, DaemonError::PolicyDenied(text) if text.starts_with(BENCH_TASK_BUDGET_EXHAUSTED)),
        "{error:?}"
    );
    assert_eq!(seen.lock().await.len(), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_task_that_is_not_running_or_whose_cost_is_unknown_spends_nothing() {
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let run_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    {
        let store = store.lock().await;
        store
            .insert_session(&rsid_store::test_support::test_session(
                session_id,
                std::env::temp_dir(),
            ))
            .unwrap();
        store
            .insert_bench_run(&bench_run(run_id, 0.40), chrono::Utc::now())
            .unwrap();
        let claim = crate::store::bench::NewBenchTask {
            trial: "t".into(),
            task: "fix-git".into(),
            compose_project: "p".into(),
            container: "c".into(),
            workdir: "/app".into(),
            user: None,
            agent_timeout_sec: 60,
            request_digest: "d".into(),
        };
        store
            .claim_bench_task(run_id, &claim, chrono::Utc::now())
            .unwrap();
        store
            .bind_bench_task_session(run_id, "t", session_id, "/tmp/trace.jsonl")
            .unwrap();
    }
    let noop: Arc<dyn crate::model_control::call_control::ModelCallControl> =
        Arc::new(crate::model_control::call_control::NoopModelCallControl);
    let guard = BenchTaskGuardedControl::new(
        Arc::clone(&noop),
        Arc::clone(&store),
        Some(session_id),
        None,
        Some(0.0),
        None,
    );
    // The stored run's cap applies (the launch carried none), and a running
    // task with no spend is admitted.
    assert_eq!(guard.refusal().await, None);

    // A completed call with no cost makes the spend unknown: fail closed.
    store
        .lock()
        .await
        .insert_bench_invocation_for_test(session_id, 1, "session.harness.turn", None, (1, 0, 1, 0))
        .unwrap();
    assert_eq!(guard.refusal().await.unwrap().code, BENCH_TASK_COST_UNKNOWN);

    // A task the adapter cancelled (or a restart interrupted) never spends,
    // whether or not a session of it is resumed.
    store
        .lock()
        .await
        .interrupt_live_bench_tasks(chrono::Utc::now())
        .unwrap();
    assert_eq!(guard.refusal().await.unwrap().code, BENCH_TASK_NOT_RUNNING);

    // A session outside any run, with no launch cap, is not capped at all.
    let free = BenchTaskGuardedControl::new(
        noop,
        Arc::clone(&store),
        Some(Uuid::new_v4()),
        None,
        Some(0.0),
        None,
    );
    assert_eq!(free.refusal().await, None);

    // An unwritable trace stops the next call: a trial without a full trace is
    // not graded.
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(TraceSink::open(&dir.path().join("t.jsonl")).unwrap());
    let traced = BenchTaskGuardedControl::new(
        Arc::new(crate::model_control::call_control::NoopModelCallControl),
        Arc::clone(&store),
        None,
        None,
        Some(0.0),
        Some(Arc::clone(&sink)),
    );
    assert_eq!(traced.refusal().await, None);
    sink.mark_failed_for_test();
    assert_eq!(
        traced.refusal().await.unwrap().code,
        rsi_common::bench::BENCH_TRACE_WRITE_FAILED
    );
}
