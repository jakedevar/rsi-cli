//! `rsi-eval bench digest` (#3195): bounded, mechanical per-trial trace excerpts of search-split
//! runs, for the Meta-Harness proposer (plan
//! `thoughts/shared/plans/2026-10-19-issue-3135-m6-meta-harness-loop.md`, D1).
//!
//! The proposer's input is the raw trace, never an LLM summary (arXiv 2603.28052, Table 3). A full
//! trace is far too large to commit or read, so this tool cuts a greppable excerpt per trial and
//! names the absolute trace path for a deeper grep of that one search trial.
//!
//! * Output: `<out>/<task>.k<attempt>.md` per trial plus `<out>/summary.json`.
//! * A trial file holds the outcome fields, the tool-call histogram, compaction events, the first
//!   error (a failed tool result, a non-zero exit or a model error) with 5 events before and 5
//!   after it, the last 30 events and the trace path.
//! * Every field is cut at [`FIELD_CAP`] characters (the agent read cap) with the cut marked in
//!   place (`…[+N chars]`). A trial file is at most [`FAILED_TRIAL_CAP`] or [`PASSED_TRIAL_CAP`]
//!   bytes, a candidate at most [`CANDIDATE_CAP`] bytes. To fit, the field cut shrinks first, then
//!   the number of last events; the budget of a candidate is shared out in proportion to the
//!   per-trial caps, trials that need less giving their surplus to the others.
//! * Source: the daemon's live `trace.jsonl` (`<bench-dir>/<run>/<trial>/trace.jsonl`, found by the
//!   `trace_sha256` of the record row), else the ATIF `trajectory.json` of the Harbor trial.
//!   With neither, the file says so; the outcome fields are still written.
//! * Held-out content never enters the candidate store: a record with any trial whose task is not
//!   in `split.json` `search` is refused with [`EvalError::HeldOutRefused`] (exit 4), before
//!   anything is read from a trace or written.
//! * Deterministic: no clock, no randomness, sorted iteration; equal inputs give equal bytes.
//!   No network and no model calls.
//!
//! Decisions recorded for #3195:
//! * "5 events around" the first error is read as 5 before and 5 after.
//! * The trace has no compaction record, so a compaction is detected from the requests: a
//!   `[Conversation summary:` / `[Previous conversation truncated` message not seen in an earlier
//!   request, a request with fewer messages than the one before, or a `compact` tool call.
//! * ATIF has no success flag; a tool result whose content starts `Exit code: N` (N != 0) is an
//!   error there.

#![allow(
    clippy::too_long_first_doc_paragraph,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use rsi_common::agent_session_events::AGENT_READ_EVENT_FIELD_MAX_CHARS;
use rsi_common::bench_record::{BenchRunRecord, TrialRow};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bench::load_record;
use crate::errors::EvalError;

/// Characters kept per field: the same cap as an agent event read.
pub const FIELD_CAP: usize = AGENT_READ_EVENT_FIELD_MAX_CHARS;
/// Bytes per failed (or invalid) trial file.
pub const FAILED_TRIAL_CAP: usize = 16 * 1024;
/// Bytes per passed trial file.
pub const PASSED_TRIAL_CAP: usize = 4 * 1024;
/// Bytes per candidate digest directory (trial files and `summary.json`).
pub const CANDIDATE_CAP: usize = 512 * 1024;
/// Events kept from the end of a trace.
pub const LAST_EVENTS: usize = 30;
/// Events kept on each side of the first error.
pub const ERROR_CONTEXT: usize = 5;
/// Compaction events listed per trial; the rest are only counted.
const MAX_COMPACTIONS: usize = 20;
/// Histogram rows listed per trial; the rest are only counted.
const MAX_HISTOGRAM_ROWS: usize = 40;
/// A trial file below this many bytes cannot hold its outcome, so such a budget is refused.
const MIN_TRIAL_BYTES: usize = 512;
/// The file name of the machine-readable summary.
pub const SUMMARY_FILE: &str = "summary.json";
const SCHEMA_VERSION: u32 = 1;

/// Shrink steps tried in order until a trial file fits its cap: (field cap in chars, last events).
const LADDER: [(usize, usize); 10] = [
    (FIELD_CAP, LAST_EVENTS),
    (1_000, LAST_EVENTS),
    (500, LAST_EVENTS),
    (250, LAST_EVENTS),
    (120, LAST_EVENTS),
    (60, LAST_EVENTS),
    (60, 15),
    (60, 8),
    (60, 3),
    (60, 0),
];

/// Inputs of one digest run.
#[derive(Debug, Clone)]
pub struct DigestArgs {
    /// `BenchRunRecord` v1 file (`record.json`).
    pub record: PathBuf,
    /// Output directory, e.g. `eval/bench/tb2/candidates/<id>/digest/`.
    pub out: PathBuf,
    /// `split.json`; only its `search` list may be digested.
    pub split: PathBuf,
    /// The daemon's bench directory (`~/.rsi/bench`).
    pub bench_dir: PathBuf,
    /// Restrict the trace search to one run directory.
    pub run: Option<String>,
    /// An additional Rust run or legacy Harbor job directory (or parent) searched for ATIF.
    pub trajectories: Option<PathBuf>,
}

/// What a digest run wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestOutcome {
    /// Trial files written (not counting `summary.json`).
    pub trial_files: usize,
    /// Bytes of every file written.
    pub total_bytes: usize,
}

// ---------------------------------------------------------------------------------------------
// Fields and events
// ---------------------------------------------------------------------------------------------

/// A text field cut at [`FIELD_CAP`] characters when read; it can be cut shorter when rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    value: String,
    /// Characters dropped before the value was stored.
    omitted: usize,
}

impl Field {
    fn new(text: &str) -> Self {
        match text.char_indices().nth(FIELD_CAP) {
            Some((at, _)) => Self {
                value: text[..at].to_string(),
                omitted: text[at..].chars().count(),
            },
            None => Self {
                value: text.to_string(),
                omitted: 0,
            },
        }
    }

    /// One line: control characters escaped, at most `cap` characters, the cut marked.
    fn render(&self, cap: usize) -> String {
        let total = self.value.chars().count();
        let (kept, extra) = match self.value.char_indices().nth(cap) {
            Some((at, _)) => (&self.value[..at], total - cap),
            None => (self.value.as_str(), 0),
        };
        let mut out = escape_line(kept);
        let omitted = self.omitted + extra;
        if omitted > 0 {
            let _ = write!(out, "…[+{omitted} chars]");
        }
        out
    }
}

fn escape_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// One trace line (or ATIF element) reduced to a few short fields.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    seq: u64,
    kind: String,
    fields: Vec<(&'static str, Field)>,
    error: bool,
}

impl Event {
    fn new(seq: u64, kind: &str) -> Self {
        Self {
            seq,
            kind: kind.to_string(),
            fields: Vec::new(),
            error: false,
        }
    }

    fn with(mut self, name: &'static str, text: &str) -> Self {
        self.fields.push((name, Field::new(text)));
        self
    }

    fn render(&self, cap: usize) -> String {
        let mut line = format!("- #{} {}", self.seq, self.kind);
        if self.error {
            line.push_str(" ERROR");
        }
        for (name, field) in &self.fields {
            let _ = write!(line, " {name}={}", field.render(cap));
        }
        line
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn str_at<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------------------------
// Accumulating a trace
// ---------------------------------------------------------------------------------------------

const SUMMARY_MARKER: &str = "[Conversation summary:";
const TRUNCATED_MARKER: &str = "[Previous conversation truncated";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ErrorWindow {
    before: Vec<Event>,
    error: Event,
    after: Vec<Event>,
}

/// Everything a trial file shows about its trace.
#[derive(Debug, Default)]
struct TraceFacts {
    events: u64,
    unparsable_lines: u64,
    tool_calls: BTreeMap<String, u64>,
    tool_errors: u64,
    compactions: Vec<Event>,
    compaction_total: u64,
    first_error: Option<ErrorWindow>,
    last: VecDeque<Event>,
    // Streaming state.
    before: VecDeque<Event>,
    seen_markers: HashSet<String>,
    prev_messages: Option<usize>,
}

impl TraceFacts {
    fn push(&mut self, event: Event) {
        self.events += 1;
        if event.error {
            self.tool_errors += 1;
        }
        if let Some(window) = &mut self.first_error {
            if window.after.len() < ERROR_CONTEXT {
                window.after.push(event.clone());
            }
        } else if event.error {
            self.first_error = Some(ErrorWindow {
                before: self.before.iter().cloned().collect(),
                error: event.clone(),
                after: Vec::new(),
            });
        }
        if self.before.len() == ERROR_CONTEXT {
            self.before.pop_front();
        }
        self.before.push_back(event.clone());
        if self.last.len() == LAST_EVENTS {
            self.last.pop_front();
        }
        self.last.push_back(event);
    }

    fn note_compaction(&mut self, event: Event) {
        self.compaction_total += 1;
        if self.compactions.len() < MAX_COMPACTIONS {
            self.compactions.push(event);
        }
    }

    /// One `trace.jsonl` line.
    fn push_trace_line(&mut self, line: &Value, index: u64) {
        let seq = line.get("seq").and_then(Value::as_u64).unwrap_or(index);
        let kind = str_at(line, "kind");
        let event = match kind {
            "model_request" => self.model_request(line, seq),
            "model_response" => model_response(line, seq),
            "model_error" => {
                let mut e = Event::new(seq, kind).with("error", &text_of(&line["error"]));
                e.error = true;
                e
            }
            "tool_call" => {
                let name = str_at(line, "name");
                *self.tool_calls.entry(name.to_string()).or_insert(0) += 1;
                let event = Event::new(seq, kind)
                    .with("name", name)
                    .with("args", &line["args"].to_string());
                if name == "compact" {
                    self.note_compaction(
                        Event::new(seq, "compact_tool").with("args", &line["args"].to_string()),
                    );
                }
                event
            }
            "tool_result" => tool_result(line, seq),
            other => Event::new(seq, if other.is_empty() { "unknown" } else { other })
                .with("line", &line.to_string()),
        };
        self.push(event);
    }

    fn model_request(&mut self, line: &Value, seq: u64) -> Event {
        let messages = line["request"]["messages"].as_array();
        let count = messages.map_or(0, Vec::len);
        let tools = line["request"]["tools"].as_array().map_or(0, Vec::len);
        let mut event = Event::new(seq, "model_request")
            .with("messages", &count.to_string())
            .with("tools", &tools.to_string());
        if let Some(last) = messages.and_then(|m| m.last()) {
            event = event
                .with("last_role", str_at(last, "role"))
                .with("last_content", &text_of(&last["content"]));
        }
        let mut new_marker: Option<(&'static str, String)> = None;
        for message in messages.into_iter().flatten() {
            let content = str_at(message, "content");
            let which = if content.starts_with(SUMMARY_MARKER) {
                "summary"
            } else if content.starts_with(TRUNCATED_MARKER) {
                "truncated"
            } else {
                continue;
            };
            let key: String = content.chars().take(200).collect();
            if self.seen_markers.insert(key) && new_marker.is_none() {
                new_marker = Some((which, content.to_string()));
            }
        }
        let shrank = self.prev_messages.filter(|prev| count < *prev);
        if new_marker.is_some() || shrank.is_some() {
            let kind = new_marker.as_ref().map_or("history_shrank", |(k, _)| *k);
            let before = self.prev_messages.unwrap_or(count);
            let mut compaction =
                Event::new(seq, kind).with("messages", &format!("{before} -> {count}"));
            if let Some((_, content)) = &new_marker {
                compaction = compaction.with("marker", content);
            }
            self.note_compaction(compaction);
        }
        self.prev_messages = Some(count);
        event
    }
}

fn model_response(line: &Value, seq: u64) -> Event {
    let response = &line["response"];
    let usage = &response["usage"];
    let mut event = Event::new(seq, "model_response")
        .with("stop", &text_of(&response["stop_reason"]))
        .with("prompt_tokens", &text_of(&usage["prompt_tokens"]))
        .with("completion_tokens", &text_of(&usage["completion_tokens"]))
        .with("cost_usd", &text_of(&usage["cost_usd"]))
        .with("content", &text_of(&response["content"]));
    let calls: Vec<String> = response["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| format!("{}({})", str_at(call, "name"), text_of(&call["arguments"])))
        .collect();
    if !calls.is_empty() {
        event = event.with("tool_calls", &calls.join("; "));
    }
    event
}

fn tool_result(line: &Value, seq: u64) -> Event {
    let result = &line["result"];
    let success = result["success"].as_bool().unwrap_or(true);
    let error_msg = result.get("error_msg").filter(|v| !v.is_null());
    let mut event = Event::new(seq, "tool_result")
        .with("name", str_at(line, "name"))
        .with("success", &success.to_string());
    if let Some(message) = error_msg {
        event = event.with("error_msg", &text_of(message));
    }
    event = event.with("output", &text_of(&result["output"]));
    event.error = !success || error_msg.is_some();
    event
}

/// An ATIF tool result carries no success flag; a non-zero exit is the only error it can show.
fn atif_content_is_error(content: &str) -> bool {
    content.strip_prefix("Exit code: ").is_some_and(|rest| {
        rest.split_whitespace()
            .next()
            .is_some_and(|code| code != "0")
    })
}

fn facts_from_trace_file(path: &Path) -> Result<TraceFacts, EvalError> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut facts = TraceFacts::default();
    let mut buf = Vec::new();
    let mut index = 0_u64;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        index += 1;
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(text) {
            Ok(line) => facts.push_trace_line(&line, index - 1),
            Err(_) => facts.unparsable_lines += 1,
        }
    }
    Ok(facts)
}

fn facts_from_trajectory(value: &Value) -> TraceFacts {
    let mut facts = TraceFacts::default();
    let mut seq = 0_u64;
    for step in value["steps"].as_array().into_iter().flatten() {
        let source = str_at(step, "source");
        let mut message =
            Event::new(seq, &format!("step_{source}")).with("message", &text_of(&step["message"]));
        if source == "agent" {
            message = message.with("model", &text_of(&step["model_name"]));
        }
        facts.push(message);
        seq += 1;
        for call in step["tool_calls"].as_array().into_iter().flatten() {
            let name = str_at(call, "function_name");
            *facts.tool_calls.entry(name.to_string()).or_insert(0) += 1;
            let args = call["arguments"].to_string();
            if name == "compact" {
                facts.note_compaction(Event::new(seq, "compact_tool").with("args", &args));
            }
            facts.push(
                Event::new(seq, "tool_call")
                    .with("name", name)
                    .with("args", &args),
            );
            seq += 1;
        }
        for result in step["observation"]["results"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let content = text_of(&result["content"]);
            let mut event = Event::new(seq, "tool_result").with("output", &content);
            event.error = atif_content_is_error(&content);
            facts.push(event);
            seq += 1;
        }
    }
    facts
}

// ---------------------------------------------------------------------------------------------
// Finding a trial's source
// ---------------------------------------------------------------------------------------------

/// Where a trial's events came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Trace(PathBuf),
    Trajectory(PathBuf),
    Unavailable(String),
}

impl Source {
    const fn label(&self) -> &'static str {
        match self {
            Self::Trace(_) => "trace.jsonl",
            Self::Trajectory(_) => "trajectory.json",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

fn sorted_children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    paths
}

fn trial_dirs_of(root: &Path, task: &str) -> Vec<PathBuf> {
    let prefix = format!("{task}__");
    let runner_prefix = format!("{task}-s");
    sorted_children(root)
        .into_iter()
        .filter(|path| {
            path.is_dir()
                && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with(&prefix)
                        || n.strip_prefix(&runner_prefix)
                            .and_then(|suffix| suffix.split_once("-r"))
                            .is_some_and(|(slot, retry)| {
                                slot.parse::<u32>().is_ok() && retry.parse::<u32>().is_ok()
                            })
                })
        })
        .collect()
}

fn hash_file(path: &Path) -> Result<String, EvalError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

struct Locator<'a> {
    args: &'a DigestArgs,
    hashes: HashMap<PathBuf, String>,
}

impl Locator<'_> {
    fn run_roots(&self) -> Vec<PathBuf> {
        self.args.run.as_ref().map_or_else(
            || sorted_children(&self.args.bench_dir),
            |run| vec![self.args.bench_dir.join(run)],
        )
    }

    fn hash(&mut self, path: &Path) -> Result<String, EvalError> {
        if let Some(hash) = self.hashes.get(path) {
            return Ok(hash.clone());
        }
        let hash = hash_file(path)?;
        self.hashes.insert(path.to_path_buf(), hash.clone());
        Ok(hash)
    }

    fn trace_candidates(&self, task: &str) -> Vec<PathBuf> {
        let roots = self.run_roots();
        roots
            .iter()
            .flat_map(|root| {
                let mut paths: Vec<_> = trial_dirs_of(root, task)
                    .into_iter()
                    .map(|dir| dir.join("trace.jsonl"))
                    .collect();
                paths.extend(
                    trial_dirs_of(&root.join("trials"), task)
                        .into_iter()
                        .map(|dir| dir.join("logs/agent/rsi-trace.jsonl")),
                );
                paths
            })
            .filter(|path| path.is_file())
            .collect()
    }

    fn trajectory_candidates(&self, task: &str) -> Vec<PathBuf> {
        let mut roots = self.run_roots();
        if let Some(root) = &self.args.trajectories {
            roots.push(root.clone());
            roots.extend(sorted_children(root).into_iter().filter(|p| p.is_dir()));
        }
        let mut paths: Vec<_> = roots
            .iter()
            .flat_map(|root| {
                let mut paths: Vec<_> = trial_dirs_of(root, task)
                    .into_iter()
                    .map(|dir| dir.join("agent/trajectory.json"))
                    .collect();
                paths.extend(
                    trial_dirs_of(&root.join("trials"), task)
                        .into_iter()
                        .map(|dir| dir.join("logs/agent/trajectory.json")),
                );
                paths
            })
            .filter(|path| path.is_file())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    fn find_trace(&mut self, row: &TrialRow) -> Result<Result<PathBuf, String>, EvalError> {
        let candidates = self.trace_candidates(&row.task);
        if candidates.is_empty() {
            return Ok(Err("no trace.jsonl found for the task".to_string()));
        }
        if let Some(want) = &row.trace_sha256 {
            let want = want.to_ascii_lowercase();
            for path in candidates {
                if self.hash(&path)? == want {
                    return Ok(Ok(path));
                }
            }
            return Ok(Err(
                "no trace.jsonl matches the row's trace_sha256".to_string()
            ));
        }
        if let [only] = candidates.as_slice() {
            Ok(Ok(only.clone()))
        } else {
            Ok(Err(
                "the row has no trace_sha256 and several traces exist".to_string()
            ))
        }
    }

    fn find_trajectory(&self, row: &TrialRow) -> Result<PathBuf, String> {
        let candidates = self.trajectory_candidates(&row.task);
        if candidates.is_empty() {
            return Err("no trajectory.json found for the task".to_string());
        }
        if let Some(session) = &row.session_id {
            for path in &candidates {
                let value = read_json(path).ok();
                if value
                    .as_ref()
                    .is_some_and(|v| str_at(v, "session_id") == session)
                {
                    return Ok(path.clone());
                }
            }
            return Err("no trajectory.json has the row's session_id".to_string());
        }
        if let [only] = candidates.as_slice() {
            Ok(only.clone())
        } else {
            Err("the row has no session_id and several trajectories exist".to_string())
        }
    }

    fn locate(&mut self, row: &TrialRow) -> Result<Source, EvalError> {
        let trace_reason = match self.find_trace(row)? {
            Ok(path) => return Ok(Source::Trace(canonical(&path))),
            Err(reason) => reason,
        };
        match self.find_trajectory(row) {
            Ok(path) => Ok(Source::Trajectory(canonical(&path))),
            Err(reason) => Ok(Source::Unavailable(format!("{trace_reason}; {reason}"))),
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn read_json(path: &Path) -> Result<Value, EvalError> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

// ---------------------------------------------------------------------------------------------
// Rendering one trial
// ---------------------------------------------------------------------------------------------

struct TrialDigest {
    row: TrialRow,
    source: Source,
    facts: TraceFacts,
    natural_cap: usize,
}

impl TrialDigest {
    fn file_name(&self) -> String {
        format!("{}.k{}.md", self.row.task, self.row.attempt)
    }

    fn render(&self, field_cap: usize, last_events: usize) -> String {
        let row = &self.row;
        let facts = &self.facts;
        let plain = |text: &str| Field::new(text).render(field_cap);
        let optional = |text: Option<&str>| text.map_or_else(|| "none".to_string(), plain);
        let mut out = String::new();
        let _ = writeln!(out, "# {} k{}\n", plain(&row.task), row.attempt);
        out.push_str("## Outcome\n\n");
        let _ = writeln!(out, "- passed: {}", row.passed);
        let _ = writeln!(
            out,
            "- reward: {} (valid: {})",
            row.reward, row.reward_valid
        );
        let _ = writeln!(
            out,
            "- failure_cause: {}",
            optional(row.failure_cause.as_deref())
        );
        let _ = writeln!(
            out,
            "- exception_type: {}",
            optional(row.exception_type.as_deref())
        );
        let _ = writeln!(
            out,
            "- stop_reason: {}",
            optional(row.stop_reason.as_deref())
        );
        let _ = writeln!(out, "- turns: {}", row.turns);
        let _ = writeln!(out, "- cost_usd: {}", row.cost_usd);
        let _ = writeln!(
            out,
            "- tokens: prompt {}, cached {}, output {}, reasoning {}",
            row.prompt_tokens, row.cached_tokens, row.output_tokens, row.reasoning_tokens
        );
        let _ = writeln!(out, "- wall_ms: {}", row.wall_ms);
        let _ = writeln!(
            out,
            "- infra_interrupted: {}, invalid_route: {}, suspected_solution_lookup: {}",
            row.infra_interrupted, row.invalid_route, row.suspected_solution_lookup
        );
        let _ = writeln!(out, "- session_id: {}", optional(row.session_id.as_deref()));
        let _ = writeln!(
            out,
            "- trace_sha256: {}",
            optional(row.trace_sha256.as_deref())
        );
        match &self.source {
            Source::Trace(path) | Source::Trajectory(path) => {
                let _ = writeln!(
                    out,
                    "- trace: {} ({})",
                    plain(&path.to_string_lossy()),
                    self.source.label()
                );
            }
            Source::Unavailable(reason) => {
                let _ = writeln!(out, "- trace: unavailable ({})", plain(reason));
            }
        }
        let _ = writeln!(
            out,
            "- events: {} (tool errors {}, unparsable lines {})\n",
            facts.events, facts.tool_errors, facts.unparsable_lines
        );

        out.push_str("## Tool calls\n\n");
        if facts.tool_calls.is_empty() {
            out.push_str("(none)\n");
        } else {
            let mut rows: Vec<(&String, &u64)> = facts.tool_calls.iter().collect();
            rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            for (name, count) in rows.iter().take(MAX_HISTOGRAM_ROWS) {
                let _ = writeln!(out, "- {}: {count}", plain(name));
            }
            if rows.len() > MAX_HISTOGRAM_ROWS {
                let _ = writeln!(out, "- …[+{} more tools]", rows.len() - MAX_HISTOGRAM_ROWS);
            }
        }

        out.push_str("\n## Compaction events\n\n");
        if facts.compaction_total == 0 {
            out.push_str("(none)\n");
        } else {
            for event in &facts.compactions {
                let _ = writeln!(out, "{}", event.render(field_cap));
            }
            let more = facts.compaction_total - facts.compactions.len() as u64;
            if more > 0 {
                let _ = writeln!(out, "- …[+{more} more compaction events]");
            }
        }

        out.push_str("\n## First error\n\n");
        match &facts.first_error {
            None => out.push_str("(none)\n"),
            Some(window) => {
                let context = match last_events {
                    n if n >= 15 => ERROR_CONTEXT,
                    n if n >= 8 => 2,
                    _ => 0,
                };
                let skip_before = window.before.len().saturating_sub(context);
                for event in &window.before[skip_before..] {
                    let _ = writeln!(out, "{}", event.render(field_cap));
                }
                let _ = writeln!(out, "{}", window.error.render(field_cap));
                for event in window.after.iter().take(context) {
                    let _ = writeln!(out, "{}", event.render(field_cap));
                }
            }
        }

        out.push_str("\n## Last events\n\n");
        let skip = facts.last.len().saturating_sub(last_events);
        if skip > 0 {
            let _ = writeln!(
                out,
                "(…{skip} older of the last {LAST_EVENTS} omitted to fit the cap)"
            );
        }
        if facts.last.is_empty() {
            out.push_str("(none)\n");
        }
        for event in facts.last.iter().skip(skip) {
            let _ = writeln!(out, "{}", event.render(field_cap));
        }
        out
    }

    /// The file text, at most `cap` bytes.
    fn fit(&self, cap: usize) -> String {
        for (field_cap, last) in LADDER {
            let text = self.render(field_cap, last);
            if text.len() <= cap {
                return text;
            }
        }
        let (field_cap, last) = LADDER[LADDER.len() - 1];
        hard_cut(&self.render(field_cap, last), cap)
    }
}

fn hard_cut(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let marker = format!("\n[digest cut to fit {cap} bytes]\n");
    let room = cap.saturating_sub(marker.len());
    let mut end = room.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str(&marker);
    out.truncate(cap);
    out
}

/// Per-trial caps summing to at most `budget` whenever the floors fit: every trial first gets
/// its floor (`MIN_TRIAL_BYTES`, or its whole size when smaller), then the bytes above the floors
/// are shared in proportion to the natural caps, each trial taking no more than its size; what
/// small trials do not need goes to the others.
fn share_budget(sizes: &[(usize, usize)], budget: usize) -> Vec<usize> {
    // sizes: (natural cap w, rendered size at that cap)
    let floors: Vec<u64> = sizes
        .iter()
        .map(|s| s.1.min(MIN_TRIAL_BYTES) as u64)
        .collect();
    // The bytes each trial wants above its floor.
    let extra: Vec<u64> = sizes
        .iter()
        .zip(&floors)
        .map(|(s, floor)| s.1 as u64 - floor)
        .collect();
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    // Ascending by extra/weight, compared without division.
    order.sort_by(|&a, &b| {
        (extra[a] * sizes[b].0 as u64)
            .cmp(&(extra[b] * sizes[a].0 as u64))
            .then(a.cmp(&b))
    });
    let mut caps: Vec<u64> = floors.clone();
    let mut left = (budget as u64).saturating_sub(floors.iter().sum::<u64>());
    let mut weight: u64 = sizes.iter().map(|s| s.0 as u64).sum();
    for &i in &order {
        let w = sizes[i].0 as u64;
        let fair = left * w / weight.max(1);
        // A trial that fits its fair share keeps its size; the others split the remainder.
        let take = extra[i].min(fair);
        caps[i] += take;
        left -= take;
        weight -= w;
    }
    caps.into_iter()
        .map(|c| usize::try_from(c).unwrap_or(usize::MAX))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The summary
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SummaryTrial {
    task: String,
    attempt: u32,
    passed: bool,
    reward: f64,
    failure_cause: Option<String>,
    stop_reason: Option<String>,
    turns: u32,
    cost_usd: f64,
    prompt_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    wall_ms: u64,
    digest_file: String,
    trace_source: &'static str,
    events: u64,
}

#[derive(Debug, Serialize)]
struct Summary {
    schema_version: u32,
    observation_fingerprint: String,
    model: String,
    split: String,
    failed_trial_cap_bytes: usize,
    passed_trial_cap_bytes: usize,
    candidate_cap_bytes: usize,
    trials: Vec<SummaryTrial>,
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

/// The `search` task names of a `split.json`.
///
/// # Errors
/// [`EvalError::Io`], [`EvalError::Json`] or [`EvalError::Bench`] for a file without a non-empty
/// `search` array of strings.
pub fn load_search_tasks(path: &Path) -> Result<BTreeSet<String>, EvalError> {
    let value = read_json(path)?;
    let tasks: BTreeSet<String> = value["search"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if tasks.is_empty() {
        return Err(EvalError::Bench(format!(
            "{}: no `search` task list",
            path.display()
        )));
    }
    Ok(tasks)
}

fn file_safe(task: &str) -> bool {
    !task.is_empty()
        && !task.starts_with('.')
        && task
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Refuses a record with any trial outside `search`, before anything is read or written.
///
/// # Errors
/// [`EvalError::HeldOutRefused`] for the first trial (in task, attempt order) outside the list;
/// [`EvalError::Bench`] for an unsafe task name or a duplicate `(task, attempt)`.
fn check_trials(
    record: &BenchRunRecord,
    search: &BTreeSet<String>,
) -> Result<Vec<TrialRow>, EvalError> {
    let mut trials = record.trials.clone();
    trials.sort_by(|a, b| (&a.task, a.attempt).cmp(&(&b.task, b.attempt)));
    if let Some(trial) = trials.iter().find(|t| !search.contains(&t.task)) {
        return Err(EvalError::HeldOutRefused {
            task: trial.task.clone(),
            attempt: trial.attempt,
        });
    }
    for pair in trials.windows(2) {
        if pair[0].task == pair[1].task && pair[0].attempt == pair[1].attempt {
            return Err(EvalError::Bench(format!(
                "record has two trials for {}#{}",
                pair[0].task, pair[0].attempt
            )));
        }
    }
    if let Some(trial) = trials.iter().find(|t| !file_safe(&t.task)) {
        return Err(EvalError::Bench(format!(
            "task name {:?} is not safe as a file name",
            trial.task
        )));
    }
    Ok(trials)
}

fn collect_digest(locator: &mut Locator<'_>, row: TrialRow) -> Result<TrialDigest, EvalError> {
    let source = locator.locate(&row)?;
    let facts = match &source {
        Source::Trace(path) => facts_from_trace_file(path)?,
        Source::Trajectory(path) => facts_from_trajectory(&read_json(path)?),
        Source::Unavailable(_) => TraceFacts::default(),
    };
    let natural_cap = if row.passed && row.is_valid() {
        PASSED_TRIAL_CAP
    } else {
        FAILED_TRIAL_CAP
    };
    Ok(TrialDigest {
        row,
        source,
        facts,
        natural_cap,
    })
}

fn is_own_output(name: &str) -> bool {
    name == SUMMARY_FILE
        || name.strip_suffix(".md").is_some_and(|stem| {
            stem.rsplit_once(".k")
                .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// Builds the digest of every trial of `args.record` into `args.out`.
///
/// # Errors
/// [`EvalError::HeldOutRefused`] when a trial's task is not in `split.json` `search` (nothing is
/// written); [`EvalError::Bench`] for an invalid record, split file or budget; [`EvalError::Io`]
/// and [`EvalError::Json`] for unreadable inputs or an unwritable output.
pub fn run_digest(args: &DigestArgs) -> Result<DigestOutcome, EvalError> {
    let record = load_record(&args.record)?;
    let search = load_search_tasks(&args.split)?;
    let trials = check_trials(&record, &search)?;

    let mut locator = Locator {
        args,
        hashes: HashMap::new(),
    };
    let mut digests = Vec::with_capacity(trials.len());
    for row in trials {
        digests.push(collect_digest(&mut locator, row)?);
    }

    let summary = Summary {
        schema_version: SCHEMA_VERSION,
        observation_fingerprint: record.run.observation_fingerprint.clone(),
        model: record.run.model.clone(),
        split: record.run.split,
        failed_trial_cap_bytes: FAILED_TRIAL_CAP,
        passed_trial_cap_bytes: PASSED_TRIAL_CAP,
        candidate_cap_bytes: CANDIDATE_CAP,
        trials: digests
            .iter()
            .map(|d| SummaryTrial {
                task: d.row.task.clone(),
                attempt: d.row.attempt,
                passed: d.row.passed,
                reward: d.row.reward,
                failure_cause: d.row.failure_cause.clone(),
                stop_reason: d.row.stop_reason.clone(),
                turns: d.row.turns,
                cost_usd: d.row.cost_usd,
                prompt_tokens: d.row.prompt_tokens,
                cached_tokens: d.row.cached_tokens,
                output_tokens: d.row.output_tokens,
                reasoning_tokens: d.row.reasoning_tokens,
                wall_ms: d.row.wall_ms,
                digest_file: d.file_name(),
                trace_source: d.source.label(),
                events: d.facts.events,
            })
            .collect(),
    };
    let mut summary_json = serde_json::to_string_pretty(&summary)?;
    summary_json.push('\n');
    let budget = CANDIDATE_CAP.saturating_sub(summary_json.len());
    if digests.len() * MIN_TRIAL_BYTES > budget {
        return Err(EvalError::Bench(format!(
            "{} trials cannot share the {CANDIDATE_CAP}-byte candidate cap",
            digests.len()
        )));
    }

    let natural: Vec<String> = digests.iter().map(|d| d.fit(d.natural_cap)).collect();
    let texts: Vec<String> = if natural.iter().map(String::len).sum::<usize>() <= budget {
        natural
    } else {
        let sizes: Vec<(usize, usize)> = digests
            .iter()
            .zip(&natural)
            .map(|(d, text)| (d.natural_cap, text.len()))
            .collect();
        let caps = share_budget(&sizes, budget);
        digests
            .iter()
            .zip(caps)
            .map(|(d, cap)| d.fit(cap.min(d.natural_cap)))
            .collect()
    };

    let total_bytes = summary_json.len() + texts.iter().map(String::len).sum::<usize>();
    if total_bytes > CANDIDATE_CAP {
        return Err(EvalError::Bench(format!(
            "digest of {total_bytes} bytes exceeds the {CANDIDATE_CAP}-byte candidate cap"
        )));
    }
    std::fs::create_dir_all(&args.out)?;
    for path in sorted_children(&args.out) {
        let own = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_own_output);
        if own && path.is_file() {
            std::fs::remove_file(path)?;
        }
    }
    for (digest, text) in digests.iter().zip(&texts) {
        std::fs::write(args.out.join(digest.file_name()), text)?;
    }
    std::fs::write(args.out.join(SUMMARY_FILE), &summary_json)?;
    Ok(DigestOutcome {
        trial_files: texts.len(),
        total_bytes,
    })
}

/// `~/.rsi/bench`, from `HOME` (or `USERPROFILE`).
///
/// # Errors
/// [`EvalError::Bench`] when no home directory is set.
pub fn default_bench_dir() -> Result<PathBuf, EvalError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".rsi").join("bench"))
        .ok_or_else(|| EvalError::Bench("HOME is not set; pass --bench-dir".to_string()))
}

#[cfg(test)]
mod tests;
