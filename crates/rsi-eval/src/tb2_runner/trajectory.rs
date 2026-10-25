//! ATIF trajectory and raw trace output of one Harness trial (#3682 R4, design #3628 "Native
//! Harness trial and recovery"; replaces `eval/harbor/rsi_harbor/atif.py`).
//!
//! The trajectory is built in Rust from the daemon's trace (`rsid::bench::trace`: one JSON object
//! per line with `seq` and `kind` of `model_request`, `model_response`, `model_error`,
//! `tool_call` or `tool_result`) and the daemon-authoritative usage of `BenchTaskFinish`. It
//! follows the shape Harbor 0.24.0's `harbor/models/trajectories` accepts (schema `ATIF-v1.8`),
//! and [`validate`] mirrors that model's rules (sequential step ids from 1, agent-only fields,
//! observation `source_call_id`s found in the same step's `tool_calls`), so a record Harbor would
//! reject fails here without importing Harbor. The full raw trace is written byte for byte next
//! to `trajectory.json`; records the trajectory does not use stay only there.
//!
//! An oracle trial has no model and gets no trajectory at all (no fabricated tokens).

use std::{collections::VecDeque, path::Path};

use rsi_common::bench::{BenchTaskStatusV1, BenchUsage};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::client::TraceOutcome;

/// Harbor 0.24.0's default (and newest accepted) trajectory schema.
pub const ATIF_SCHEMA_VERSION: &str = "ATIF-v1.8";
/// Agent name written into the trajectory.
pub const AGENT_NAME: &str = "rsi-harness";
/// File names inside the trial's agent log directory.
pub const TRAJECTORY_FILE: &str = "trajectory.json";
pub const TRACE_FILE: &str = "rsi-trace.jsonl";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallRecord {
    pub tool_call_id: String,
    pub function_name: String,
    pub arguments: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservationResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Observation {
    pub results: Vec<ObservationResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    pub step_id: u64,
    pub source: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentInfo {
    pub name: &'static str,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FinalMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    pub total_steps: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trajectory {
    pub schema_version: &'static str,
    pub session_id: String,
    pub agent: AgentInfo,
    pub steps: Vec<Step>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub final_metrics: FinalMetrics,
}

/// Inputs of [`build_trajectory`] other than the trace.
#[derive(Debug, Clone)]
pub struct TrajectoryInputs<'a> {
    /// The instruction handed to `BenchTaskStart`; it is step 1.
    pub instruction: &'a str,
    /// The daemon's final status from Finish (usage, route, stop reason, session).
    pub status: &'a BenchTaskStatusV1,
    pub agent_version: &'a str,
}

fn non_zero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

fn raw_metrics(usage: &Value) -> Option<Metrics> {
    let object = usage.as_object()?;
    let count = |name: &str| object.get(name).and_then(Value::as_u64);
    let metrics = Metrics {
        prompt_tokens: count("prompt_tokens"),
        completion_tokens: count("completion_tokens"),
        cached_tokens: count("cache_read_tokens").and_then(non_zero),
        cost_usd: object.get("cost_usd").and_then(Value::as_f64),
    };
    (metrics.prompt_tokens.is_some()
        || metrics.completion_tokens.is_some()
        || metrics.cost_usd.is_some())
    .then_some(metrics)
}

fn arguments_of(raw: &Value) -> Map<String, Value> {
    match raw {
        Value::Object(map) => map.clone(),
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => map,
            Ok(other) => Map::from_iter([("input".to_owned(), other)]),
            Err(_) => Map::from_iter([("input".to_owned(), Value::String(text.clone()))]),
        },
        Value::Null => Map::new(),
        other => Map::from_iter([("input".to_owned(), other.clone())]),
    }
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

struct Pending {
    step: usize,
    call_id: String,
    name: String,
}

#[derive(Default)]
struct Builder {
    steps: Vec<Step>,
    pending: VecDeque<Pending>,
    model: Option<String>,
    first_model: Option<String>,
    skipped_lines: u64,
}

impl Builder {
    fn push(&mut self, mut step: Step) -> usize {
        step.step_id = self.steps.len() as u64 + 1;
        self.steps.push(step);
        self.steps.len() - 1
    }

    fn step(source: &'static str, message: String) -> Step {
        Step {
            step_id: 0,
            source,
            message,
            model_name: None,
            reasoning_content: None,
            tool_calls: None,
            observation: None,
            metrics: None,
            extra: None,
        }
    }

    fn add_result(&mut self, index: usize, result: ObservationResult) {
        let step = &mut self.steps[index];
        match step.observation.as_mut() {
            Some(observation) => observation.results.push(result),
            None => {
                step.observation = Some(Observation {
                    results: vec![result],
                });
            }
        }
    }

    fn model_response(&mut self, record: &Value) {
        let Some(response) = record.get("response").filter(|r| r.is_object()) else {
            self.skipped_lines += 1;
            return;
        };
        let mut step = Self::step("agent", text_of(response.get("content")));
        step.model_name.clone_from(&self.model);
        step.reasoning_content =
            Some(text_of(response.get("reasoning_content"))).filter(|t| !t.is_empty());
        step.metrics = response.get("usage").and_then(raw_metrics);
        let mut extra = Map::new();
        if let Some(stop) = response.get("stop_reason").filter(|s| !s.is_null()) {
            extra.insert("stop_reason".into(), stop.clone());
        }
        if let Some(usage) = response.get("usage").and_then(Value::as_object) {
            for key in ["reasoning_tokens", "cache_creation_tokens"] {
                if let Some(count) = usage.get(key).and_then(Value::as_u64).and_then(non_zero) {
                    extra.insert(key.into(), json!(count));
                }
            }
        }
        step.extra = (!extra.is_empty()).then(|| Value::Object(extra));
        let mut calls = Vec::new();
        let mut hosted = Vec::new();
        let raw_calls = response.get("tool_calls").and_then(Value::as_array);
        for (n, raw) in raw_calls.into_iter().flatten().enumerate() {
            let id = raw
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map_or_else(
                    || format!("call_{}_{}", self.steps.len() + 1, n + 1),
                    str::to_owned,
                );
            let name = text_of(raw.get("name"));
            if let Some(result) = raw.get("hosted_result").filter(|r| !r.is_null()) {
                hosted.push((id.clone(), text_of(Some(result))));
            }
            calls.push(ToolCallRecord {
                tool_call_id: id,
                function_name: if name.is_empty() {
                    "unknown".into()
                } else {
                    name
                },
                arguments: arguments_of(raw.get("arguments").unwrap_or(&Value::Null)),
            });
        }
        let ids: Vec<(String, String)> = calls
            .iter()
            .map(|c| (c.tool_call_id.clone(), c.function_name.clone()))
            .collect();
        step.tool_calls = (!calls.is_empty()).then_some(calls);
        let index = self.push(step);
        for (call_id, content) in hosted {
            self.add_result(
                index,
                ObservationResult {
                    source_call_id: Some(call_id),
                    content,
                    extra: None,
                },
            );
        }
        for (call_id, name) in ids {
            // Provider-hosted calls already carry their result; the loop never executes them.
            let answered = self.steps[index].observation.as_ref().is_some_and(|o| {
                o.results
                    .iter()
                    .any(|r| r.source_call_id.as_deref() == Some(call_id.as_str()))
            });
            if !answered {
                self.pending.push_back(Pending {
                    step: index,
                    call_id,
                    name,
                });
            }
        }
    }

    fn tool_result(&mut self, record: &Value) {
        let name = text_of(record.get("name"));
        let result = record.get("result").unwrap_or(&Value::Null);
        let mut extra = Map::new();
        extra.insert(
            "success".into(),
            result.get("success").cloned().unwrap_or(Value::Null),
        );
        if let Some(error) = result.get("error_msg").filter(|e| !e.is_null()) {
            extra.insert("error_msg".into(), error.clone());
        }
        let mut observed = ObservationResult {
            source_call_id: None,
            content: text_of(result.get("output")),
            extra: Some(Value::Object(extra)),
        };
        // Results carry only the tool name, so a result answers the oldest unanswered call of that
        // name, in the order the loop dispatched them.
        let matched = self
            .pending
            .iter()
            .position(|p| p.name == name)
            .and_then(|at| self.pending.remove(at));
        if let Some(pending) = matched {
            observed.source_call_id = Some(pending.call_id);
            self.add_result(pending.step, observed);
            return;
        }
        // An orphan result still belongs in the record: attach it to the latest agent step.
        let index = match self.steps.iter().rposition(|s| s.source == "agent") {
            Some(index) => index,
            None => self.push(Self::step("agent", String::new())),
        };
        self.add_result(index, observed);
    }

    fn feed(&mut self, line: &str) {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            self.skipped_lines += 1;
            return;
        };
        match record.get("kind").and_then(Value::as_str) {
            Some("model_request") => {
                let model = record
                    .pointer("/request/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if self.first_model.is_none() {
                    self.first_model.clone_from(&model);
                }
                self.model = model;
            }
            Some("model_response") => self.model_response(&record),
            Some("tool_result") => self.tool_result(&record),
            Some("model_error") => {
                let message = format!("model error: {}", text_of(record.get("error")));
                self.push(Self::step("system", message));
            }
            // tool_call lines repeat what the model response already carries.
            Some("tool_call") => {}
            _ => self.skipped_lines += 1,
        }
    }
}

fn final_metrics(usage: &BenchUsage, steps: u64) -> FinalMetrics {
    FinalMetrics {
        total_prompt_tokens: Some(usage.prompt_tokens),
        total_completion_tokens: Some(usage.output_tokens),
        total_cached_tokens: Some(usage.cached_tokens),
        total_cost_usd: usage.cost_usd,
        total_steps: steps,
    }
}

/// Build the trajectory. Step 1 is always the task instruction. An unavailable trace yields the
/// instruction step alone with the reason in `notes`; usage always comes from the daemon.
#[must_use]
pub fn build_trajectory(inputs: &TrajectoryInputs<'_>, trace: &TraceOutcome) -> Trajectory {
    let mut builder = Builder::default();
    builder.push(Builder::step("user", inputs.instruction.to_owned()));
    let mut notes = None;
    match trace {
        TraceOutcome::Complete { jsonl, .. } => {
            for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
                builder.feed(line);
            }
            if builder.skipped_lines > 0 {
                notes = Some(format!(
                    "{} trace line(s) were not mapped into steps; see {TRACE_FILE}",
                    builder.skipped_lines
                ));
            }
        }
        TraceOutcome::Unavailable { reason } => {
            notes = Some(format!("trace unavailable: {reason}"))
        }
    }
    let status = inputs.status;
    let mut extra = Map::new();
    for (key, value) in [
        ("harness_build_digest", status.harness_build_digest.as_ref()),
        ("route_verdict", status.route_verdict.as_ref()),
        ("stop_reason", status.stop_reason.as_ref()),
    ] {
        if let Some(value) = value {
            extra.insert(key.into(), json!(value));
        }
    }
    extra.insert("trial".into(), json!(status.trial));
    extra.insert("task".into(), json!(status.task));
    let steps_total = builder.steps.len() as u64;
    Trajectory {
        schema_version: ATIF_SCHEMA_VERSION,
        session_id: status
            .session_id
            .clone()
            .unwrap_or_else(|| status.trial.clone()),
        agent: AgentInfo {
            name: AGENT_NAME,
            version: inputs.agent_version.to_owned(),
            model_name: builder.first_model.clone(),
            extra: Some(Value::Object(extra)),
        },
        final_metrics: final_metrics(&status.usage, steps_total),
        steps: builder.steps,
        notes,
    }
}

/// The rules of Harbor 0.24.0's trajectory model that matter here.
///
/// # Errors
/// The first rule the trajectory breaks.
pub fn validate(trajectory: &Trajectory) -> Result<(), String> {
    for (index, step) in trajectory.steps.iter().enumerate() {
        let expected = index as u64 + 1;
        if step.step_id != expected {
            return Err(format!(
                "steps[{index}].step_id: expected {expected} (sequential from 1), got {}",
                step.step_id
            ));
        }
        if step.source != "agent"
            && (step.model_name.is_some()
                || step.reasoning_content.is_some()
                || step.tool_calls.is_some()
                || step.metrics.is_some())
        {
            return Err(format!(
                "step {}: agent-only field on a {} step",
                step.step_id, step.source
            ));
        }
        if let Some(observation) = &step.observation {
            for result in &observation.results {
                let Some(id) = result.source_call_id.as_deref() else {
                    continue;
                };
                let found = step
                    .tool_calls
                    .iter()
                    .flatten()
                    .any(|call| call.tool_call_id == id);
                if !found {
                    return Err(format!(
                        "step {}: observation references source_call_id {id:?} not in its tool_calls",
                        step.step_id
                    ));
                }
            }
        }
    }
    if trajectory.final_metrics.total_steps != trajectory.steps.len() as u64 {
        return Err("final_metrics.total_steps differs from the number of steps".into());
    }
    Ok(())
}

/// Where [`write_outputs`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenOutputs {
    pub trajectory: std::path::PathBuf,
    /// `None` when the trace was unavailable: no empty file stands in for it.
    pub trace: Option<std::path::PathBuf>,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Write `trajectory.json` and the raw `rsi-trace.jsonl` (byte for byte) into the trial's agent
/// log directory. The trajectory is validated first.
///
/// # Errors
/// A rule violation from [`validate`] or an I/O error.
pub fn write_outputs(
    logs_dir: &Path,
    trajectory: &Trajectory,
    trace: &TraceOutcome,
) -> Result<WrittenOutputs, String> {
    validate(trajectory)?;
    std::fs::create_dir_all(logs_dir).map_err(|e| format!("{}: {e}", logs_dir.display()))?;
    let mut text = serde_json::to_string_pretty(trajectory).map_err(|e| e.to_string())?;
    text.push('\n');
    let path = logs_dir.join(TRAJECTORY_FILE);
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    let trace_path = match trace {
        TraceOutcome::Complete { jsonl, .. } => {
            let target = logs_dir.join(TRACE_FILE);
            write_atomic(&target, jsonl.as_bytes())
                .map_err(|e| format!("{}: {e}", target.display()))?;
            Some(target)
        }
        TraceOutcome::Unavailable { .. } => None,
    };
    Ok(WrittenOutputs {
        trajectory: path,
        trace: trace_path,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rsi_common::bench::BenchTaskState;

    fn status() -> BenchTaskStatusV1 {
        BenchTaskStatusV1 {
            run_id: "run-1".into(),
            task: "fix-git".into(),
            trial: "fix-git__r1".into(),
            attempt: 1,
            state: BenchTaskState::Finished,
            stop_reason: Some("end_turn".into()),
            session_id: Some("sess-1".into()),
            usage: BenchUsage {
                cost_usd: Some(0.0123),
                prompt_tokens: 1200,
                cached_tokens: 900,
                output_tokens: 300,
                reasoning_tokens: 5,
                wall_ms: 4000,
                calls: 2,
            },
            route_verdict: Some("valid".into()),
            route_violations: Vec::new(),
            harness_build_digest: Some("sha256:build".into()),
            trace: None,
        }
    }

    fn complete(lines: &[Value]) -> TraceOutcome {
        TraceOutcome::Complete {
            jsonl: lines.iter().map(|l| format!("{l}\n")).collect(),
            sha256: String::new(),
        }
    }

    fn response(content: &str, calls: &Value, usage: &Value) -> Value {
        json!({"seq": 1, "kind": "model_response", "response": {
            "content": content, "reasoning_content": null, "tool_calls": calls,
            "stop_reason": "stop", "usage": usage}})
    }

    fn inputs(status: &BenchTaskStatusV1) -> TrajectoryInputs<'_> {
        TrajectoryInputs {
            instruction: "Fix the repo.",
            status,
            agent_version: "1.0",
        }
    }

    #[test]
    fn calls_results_metrics_and_daemon_usage_are_mapped() {
        let usage = json!({"prompt_tokens": 700, "completion_tokens": 20, "cache_read_tokens": 500,
                           "cache_creation_tokens": 0, "reasoning_tokens": 5, "cost_usd": 0.001});
        let trace = complete(&[
            json!({"seq": 0, "kind": "model_request", "request": {"model": "vendor/model-a"}}),
            response(
                "Looking.",
                &json!([
                    {"id": "c1", "name": "shell", "arguments": "{\"command\":\"ls\"}"},
                    {"id": "c2", "name": "shell", "arguments": "not json"}
                ]),
                &usage,
            ),
            json!({"seq": 2, "kind": "tool_call", "name": "shell", "args": {"command": "ls"}}),
            json!({"seq": 3, "kind": "tool_result", "name": "shell",
                   "result": {"success": true, "output": "a.txt\n", "error_msg": null}}),
            json!({"seq": 4, "kind": "tool_result", "name": "shell",
                   "result": {"success": false, "output": "", "error_msg": "boom"}}),
            response(
                "Done.",
                &json!([]),
                &json!({"prompt_tokens": 500, "completion_tokens": 10}),
            ),
        ]);
        let st = status();
        let trajectory = build_trajectory(&inputs(&st), &trace);
        validate(&trajectory).unwrap();
        let [user, first, second] = trajectory.steps.as_slice() else {
            panic!("{:?}", trajectory.steps);
        };
        assert_eq!(
            (user.source, user.message.as_str()),
            ("user", "Fix the repo.")
        );
        assert_eq!(first.model_name.as_deref(), Some("vendor/model-a"));
        let calls = first.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].arguments["command"], "ls");
        assert_eq!(
            calls[1].arguments["input"], "not json",
            "unparsable arguments are kept"
        );
        let results = &first.observation.as_ref().unwrap().results;
        assert_eq!(results[0].source_call_id.as_deref(), Some("c1"));
        assert_eq!(results[0].content, "a.txt\n");
        assert_eq!(results[1].source_call_id.as_deref(), Some("c2"));
        assert_eq!(results[1].extra.as_ref().unwrap()["error_msg"], "boom");
        let metrics = first.metrics.as_ref().unwrap();
        assert_eq!(
            (metrics.prompt_tokens, metrics.cached_tokens),
            (Some(700), Some(500))
        );
        assert_eq!(second.message, "Done.");
        // Final metrics are the daemon's, not a sum of trace usage.
        let total = &trajectory.final_metrics;
        assert_eq!(total.total_prompt_tokens, Some(1200));
        assert_eq!(total.total_cached_tokens, Some(900));
        assert_eq!(total.total_completion_tokens, Some(300));
        assert_eq!(total.total_cost_usd, Some(0.0123));
        assert_eq!(total.total_steps, 3);
        assert_eq!(trajectory.session_id, "sess-1");
        assert_eq!(trajectory.schema_version, "ATIF-v1.8");
        assert!(trajectory.notes.is_none());
    }

    #[test]
    fn orphans_errors_and_unknown_records_never_break_the_trajectory() {
        let trace = TraceOutcome::Complete {
            jsonl: [
                json!({"seq": 0, "kind": "tool_result", "name": "shell",
                       "result": {"success": true, "output": "early"}})
                .to_string(),
                "{not json".into(),
                json!({"seq": 1, "kind": "model_error", "error": "rate limited"}).to_string(),
                json!({"seq": 2, "kind": "something_new"}).to_string(),
            ]
            .join("\n"),
            sha256: String::new(),
        };
        let st = status();
        let trajectory = build_trajectory(&inputs(&st), &trace);
        validate(&trajectory).unwrap();
        // user, an agent step holding the orphan result, the model error as a system step.
        assert_eq!(trajectory.steps.len(), 3);
        assert_eq!(trajectory.steps[2].source, "system");
        assert!(trajectory.steps[2].message.contains("rate limited"));
        assert!(
            trajectory
                .notes
                .as_deref()
                .unwrap()
                .contains("2 trace line(s)")
        );
    }

    #[test]
    fn an_unavailable_trace_is_explicit_and_fabricates_nothing() {
        let st = status();
        let trace = TraceOutcome::Unavailable {
            reason: "no trace".into(),
        };
        let trajectory = build_trajectory(&inputs(&st), &trace);
        validate(&trajectory).unwrap();
        assert_eq!(trajectory.steps.len(), 1);
        assert_eq!(
            trajectory.notes.as_deref(),
            Some("trace unavailable: no trace")
        );
        assert_eq!(trajectory.final_metrics.total_prompt_tokens, Some(1200));
        let dir = tempfile::tempdir().unwrap();
        let written = write_outputs(dir.path(), &trajectory, &trace).unwrap();
        assert!(written.trace.is_none(), "no empty trace file stands in");
        assert!(!dir.path().join(TRACE_FILE).exists());
    }

    #[test]
    fn the_raw_trace_is_written_byte_for_byte_beside_a_valid_trajectory() {
        let st = status();
        let raw = "{\"seq\":0,\"kind\":\"model_request\",\"request\":{\"model\":\"m\"}}\n\
                   {\"seq\":1,\"kind\":\"unmapped\",\"x\":\"é\"}\n";
        let trace = TraceOutcome::Complete {
            jsonl: raw.into(),
            sha256: String::new(),
        };
        let trajectory = build_trajectory(&inputs(&st), &trace);
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("agent");
        let written = write_outputs(&logs, &trajectory, &trace).unwrap();
        assert_eq!(
            std::fs::read(written.trace.unwrap()).unwrap(),
            raw.as_bytes()
        );
        let text = std::fs::read_to_string(written.trajectory).unwrap();
        assert!(text.ends_with('\n'));
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["agent"]["name"], "rsi-harness");
        assert_eq!(parsed["agent"]["extra"]["route_verdict"], "valid");
        assert!(
            parsed
                .get("notes")
                .is_some_and(|n| n.as_str().unwrap().contains("not mapped"))
        );
    }

    #[test]
    fn validation_mirrors_the_harbor_rules() {
        let st = status();
        let mut trajectory = build_trajectory(
            &inputs(&st),
            &TraceOutcome::Unavailable { reason: "x".into() },
        );
        validate(&trajectory).unwrap();
        trajectory.steps[0].model_name = Some("m".into());
        assert!(validate(&trajectory).unwrap_err().contains("agent-only"));
        trajectory.steps[0].model_name = None;
        trajectory.steps[0].step_id = 2;
        assert!(validate(&trajectory).unwrap_err().contains("sequential"));
        trajectory.steps[0].step_id = 1;
        trajectory.steps[0].observation = Some(Observation {
            results: vec![ObservationResult {
                source_call_id: Some("ghost".into()),
                content: String::new(),
                extra: None,
            }],
        });
        assert!(
            validate(&trajectory)
                .unwrap_err()
                .contains("source_call_id")
        );
        // write_outputs refuses what validate refuses.
        let dir = tempfile::tempdir().unwrap();
        assert!(
            write_outputs(
                dir.path(),
                &trajectory,
                &TraceOutcome::Unavailable { reason: "x".into() }
            )
            .is_err()
        );
        assert!(!dir.path().join(TRAJECTORY_FILE).exists());
    }
}
