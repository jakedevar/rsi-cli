//! The live bench trace (#3178 item 6).
//!
//! A bench session's daemon writes, as the loop runs, every model request as
//! sent, every model response as received and every tool call with the result
//! the model saw, one JSON object per line, to
//! `~/.rsi/bench/<run>/<trial>/trace.jsonl`. This is the only complete record:
//! `conversation_events` clips tool results at 32 KiB and the agent read
//! clips every field at 2,000 characters, so a trace cannot be rebuilt from
//! them. The sha256 of the finished file is recorded at finish.
//!
//! Lines carry no timestamps, so two runs against the same transcript write
//! the same bytes.

use crate::session::harness::types::{ChatRequest, ChatResponse, ToolResult};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Trace files whose sink lost a line. A trial's settlement reads this, so a
/// trace with a gap is never settled as a normal completion even when the
/// failed append was the last thing the loop wrote (#3349).
fn failed_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static FAILED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    FAILED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Whether the sink that writes `path` lost a line (so the file is not a full
/// trace). Read at settlement, after the session ended.
#[must_use]
pub fn path_write_failed(path: &Path) -> bool {
    failed_paths()
        .lock()
        .map_or(true, |failed| failed.contains(path))
}

/// Append-only JSONL writer for one session.
pub struct TraceSink {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    seq: AtomicU64,
    failed: AtomicBool,
    /// Test seam: every line from this sequence number on cannot be written.
    #[cfg(test)]
    fail_from_seq: AtomicU64,
}

impl std::fmt::Debug for TraceSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceSink")
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .field("failed", &self.failed.load(Ordering::Relaxed))
            .finish()
    }
}

impl TraceSink {
    /// Open `path` for append, creating it (and its directory, mode 0700).
    ///
    /// # Errors
    /// The file or its directory cannot be created.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            seq: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            #[cfg(test)]
            fail_from_seq: AtomicU64::new(u64::MAX),
        })
    }

    /// Whether every line so far reached the file. A trace with a gap is not a
    /// full trace; the loop refuses its next model call and fails a completed
    /// call once this is false, and settlement refuses to settle the trial as
    /// a normal completion ([`path_write_failed`]).
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }

    /// Test seam: behave as if a line could not be written.
    #[cfg(test)]
    pub(crate) fn mark_failed_for_test(&self) {
        self.mark_failed();
    }

    /// Test seam: the append of line `seq` (and every later one) fails.
    #[cfg(test)]
    pub(crate) fn fail_writes_from_for_test(&self, seq: u64) {
        self.fail_from_seq.store(seq, Ordering::SeqCst);
    }

    fn mark_failed(&self) {
        self.failed.store(true, Ordering::SeqCst);
        if let Ok(mut failed) = failed_paths().lock() {
            failed.insert(self.path.clone());
        }
    }

    fn write(&self, kind: &str, body: Value) {
        let mut line = json!({"seq": 0, "kind": kind});
        if let (Some(object), Some(extra)) = (line.as_object_mut(), body.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        let Ok(mut file) = self.file.lock() else {
            self.mark_failed();
            return;
        };
        // The sequence number and the append are one step, so lines are in
        // sequence order even when a parallel tool batch records at once.
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        line["seq"] = json!(seq);
        let mut text = line.to_string();
        text.push('\n');
        #[cfg(test)]
        let injected = seq >= self.fail_from_seq.load(Ordering::SeqCst);
        #[cfg(not(test))]
        let injected = false;
        let written = if injected {
            Err(std::io::Error::other("injected trace write failure"))
        } else {
            file.write_all(text.as_bytes()).and_then(|()| file.flush())
        };
        if let Err(error) = written {
            tracing::error!(%error, "bench trace write failed; the trace is incomplete");
            self.mark_failed();
        }
    }

    /// A model request as the provider is handed it.
    pub fn model_request(&self, request: &ChatRequest) {
        self.write(
            "model_request",
            json!({"request": {
                "model": request.model,
                "messages": request.messages,
                "tools": request.tools,
                "temperature": request.temperature,
                "max_tokens": request.max_tokens,
                "stream": request.stream,
                "reasoning_effort": request.reasoning_effort,
                "context_editing": request.context_editing,
                "tool_choice_none": request.tool_choice_none,
            }}),
        );
    }

    /// A model response as the provider returned it.
    pub fn model_response(&self, response: &ChatResponse) {
        self.write(
            "model_response",
            json!({"response": {
                "content": response.content,
                "tool_calls": response.tool_calls,
                "reasoning_content": response.reasoning_content,
                "stop_reason": response.stop_reason,
                "usage": {
                    "prompt_tokens": response.usage.prompt_tokens,
                    "completion_tokens": response.usage.completion_tokens,
                    "total_tokens": response.usage.total_tokens,
                    "cache_creation_tokens": response.usage.cache_creation_tokens,
                    "cache_read_tokens": response.usage.cache_read_tokens,
                    "reasoning_tokens": response.usage.reasoning_tokens,
                    "cost_usd": response.usage.cost_usd,
                },
            }}),
        );
    }

    /// A model call that failed.
    pub fn model_error(&self, error: &str) {
        self.write("model_error", json!({"error": error}));
    }

    /// A tool call as the loop dispatches it.
    pub fn tool_call(&self, name: &str, args: &Value) {
        self.write("tool_call", json!({"name": name, "args": args}));
    }

    /// The result the model saw for a tool call.
    pub fn tool_result(&self, name: &str, result: &ToolResult) {
        self.write(
            "tool_result",
            json!({"name": name, "result": {
                "success": result.success,
                "output": result.output,
                "error_msg": result.error_msg,
            }}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::harness::types::{ChatMessage, TokenUsage};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn lines_are_sequenced_json_in_the_order_they_were_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t").join("trace.jsonl");
        let sink = TraceSink::open(&path).unwrap();
        assert!(sink.healthy());
        sink.model_request(&ChatRequest {
            messages: vec![ChatMessage::user("hi")],
            model: "m".into(),
            temperature: None,
            max_tokens: Some(10),
            tools: Vec::new(),
            stream: false,
            reasoning_effort: Some("high".into()),
            context_editing: false,
            tool_choice_none: false,
        });
        sink.model_response(&ChatResponse {
            content: "done".into(),
            tool_calls: Vec::new(),
            usage: TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
                cost_usd: Some(0.002),
                ..TokenUsage::default()
            },
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: Some("stop".into()),
        });
        sink.tool_call("shell", &json!({"command": "ls"}));
        sink.tool_result(
            "shell",
            &ToolResult {
                success: false,
                output: "x".repeat(100_000),
                error_msg: Some("boom".into()),
            },
        );
        sink.model_error("rate limited");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
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
                "model_error"
            ]
        );
        for (n, line) in lines.iter().enumerate() {
            assert_eq!(line["seq"], n);
        }
        assert_eq!(lines[0]["request"]["messages"][0]["content"], "hi");
        assert_eq!(lines[1]["response"]["usage"]["cost_usd"], 0.002);
        // A tool result is never clipped: the whole 100 KB body is on the line.
        assert_eq!(
            lines[3]["result"]["output"].as_str().unwrap().len(),
            100_000
        );
        assert_eq!(lines[3]["result"]["error_msg"], "boom");
    }
}
