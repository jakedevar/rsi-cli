use crate::claude::{LaunchConfig, StreamEvent};
use crate::error::{DaemonError, Result};
use crate::model_control::ModelExecutionCapability;
use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use crate::process_control::{
    CaptureError, CaptureLimits, LOCAL_TOOL_MAX_STREAM_BYTES, bounded_lossy_concat, capture_bounded,
};
use futures::StreamExt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Wraps a running OpenAI-compatible API session (tokio task, not a subprocess).
pub struct OpenAiProcess {
    task_handle: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

impl OpenAiProcess {
    /// Request graceful cancellation of the agentic loop.
    pub fn interrupt(&self) -> Result<()> {
        self.cancel.cancel();
        Ok(())
    }

    /// Force-abort the task.
    pub async fn kill(&mut self) -> Result<()> {
        self.cancel.cancel();
        self.task_handle.abort();
        Ok(())
    }

    /// Check if the underlying tokio task has completed.
    /// Returns `true` if the task has finished (successfully, via panic, or via abort).
    pub fn is_finished(&self) -> bool {
        self.task_handle.is_finished()
    }
}

#[cfg(test)]
impl OpenAiProcess {
    /// Test-only: wrap a caller-supplied task handle so `is_alive()` (=
    /// `!task_handle.is_finished()`) is deterministically controllable without a
    /// provider binary. A never-finishing handle => alive; a completed handle =>
    /// dead. Used by the single-flight spawn-guard tests.
    pub(crate) fn test_task(task_handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            task_handle,
            cancel: CancellationToken::new(),
        }
    }
}

/// Client for OpenAI-compatible API endpoints (Ollama, OpenRouter, etc.).
pub struct OpenAiClient {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl OpenAiClient {
    /// Create a client with explicit configuration.
    pub fn with_config(base_url: String, api_key: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| DaemonError::Process(format!("Failed to create HTTP client: {e}")))?;

        tracing::info!(base_url = %base_url, has_key = api_key.is_some(), "OpenAI-compatible client configured");
        Ok(Self {
            base_url,
            api_key,
            http,
        })
    }

    /// Create a client for local models (Ollama, llama.cpp, etc.).
    /// Reads `LOCAL_LLM_BASE_URL` from environment, defaults to Ollama.
    pub fn new_local() -> Result<Self> {
        let base_url = std::env::var("LOCAL_LLM_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
        let api_key = std::env::var("LOCAL_LLM_API_KEY").ok();
        Self::with_config(base_url, api_key)
    }

    pub(crate) fn resolved_provider_label(&self) -> &'static str {
        let is_loopback = reqwest::Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .is_some_and(|host| {
                let normalized_host = host
                    .strip_prefix('[')
                    .and_then(|host| host.strip_suffix(']'))
                    .unwrap_or(&host);
                normalized_host.eq_ignore_ascii_case("localhost")
                    || normalized_host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            });
        if is_loopback {
            "Local"
        } else {
            "OpenAICompatible"
        }
    }

    /// Check if a local model server is reachable.
    pub async fn is_local_available() -> bool {
        if std::env::var("LOCAL_LLM_BASE_URL").is_ok() {
            return true;
        }
        // Try connecting to Ollama's default port — async, won't block tokio
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::net::TcpStream::connect("127.0.0.1:11434"),
        )
        .await
        .is_ok()
    }

    /// Discover available models via GET /v1/models.
    pub async fn discover_models(&self) -> Vec<(String, String)> {
        let url = format!("{}/models", self.base_url);
        let mut req = self.http.get(&url);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<Value>().await {
                    if let Some(data) = body.get("data").and_then(|v| v.as_array()) {
                        return data
                            .iter()
                            .filter_map(|m| {
                                let id = m.get("id")?.as_str()?.to_string();
                                let display = id.clone();
                                Some((id, display))
                            })
                            .collect();
                    }
                }
                Vec::new()
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "Failed to discover models");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to connect to OpenAI-compatible API");
                Vec::new()
            }
        }
    }

    /// Launch an agentic session. Returns a process handle and event receiver.
    pub fn launch(
        &self,
        config: &LaunchConfig,
        model_call_control: Arc<dyn ModelCallControl>,
        launch_invocation_id: uuid::Uuid,
    ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)> {
        let (event_tx, event_rx) = mpsc::channel(100);
        let cancel = CancellationToken::new();

        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let http = self.http.clone();
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "gemma4:e4b".to_string());
        let query = config.query.clone();
        let system_prompt = config.system_prompt.clone();
        let working_dir = config
            .working_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp")));
        let conversation_history = config.conversation_history.clone();
        let rsi_session_id = config.rsi_session_id;
        let cancel_clone = cancel.clone();

        let task_handle = tokio::spawn(async move {
            if let Err(e) = run_agentic_loop(
                &http,
                &base_url,
                api_key.as_deref(),
                &model,
                &query,
                system_prompt.as_deref(),
                &working_dir,
                conversation_history.as_deref(),
                rsi_session_id,
                launch_invocation_id,
                &event_tx,
                &cancel_clone,
                model_call_control,
            )
            .await
            {
                tracing::error!(error = %e, "OpenAI agentic loop failed");
                let _ = event_tx
                    .send(StreamEvent {
                        event_type: "system".to_string(),
                        data: serde_json::json!({
                            "subtype": "error",
                            "error": e.to_string(),
                        }),
                    })
                    .await;
            }
        });

        Ok((
            OpenAiProcess {
                task_handle,
                cancel,
            },
            event_rx,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tool_schemas() -> &'static Value {
    use std::sync::OnceLock;

    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file at the given path (relative to working directory).",
                "parameters": {
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to working directory"
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file at the given path (relative to working directory). Creates parent directories if needed.",
                "parameters": {
                    "type": "object",
                    "required": ["path", "content"],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to working directory"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a bash command in the working directory. Returns stdout and stderr.",
                "parameters": {
                    "type": "object",
                    "required": ["command"],
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The bash command to execute"
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 120000,
                            "description": "Timeout in milliseconds (default and maximum: 120000)"
                        }
                    }
                }
            }
        }
    ])
    })
}

// ---------------------------------------------------------------------------
// Tool execution (sandboxed to working_dir)
// ---------------------------------------------------------------------------

use crate::path_safety::resolve_sandboxed_path;

const MAX_READ_BYTES: usize = LOCAL_TOOL_MAX_STREAM_BYTES;
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
const MAX_BASH_TIMEOUT_MS: u64 = 120_000;
const BASH_OUTPUT_TRUNCATED_MARKER: &str = "\n...[truncated]";

async fn exec_read_file(working_dir: &Path, args: &Value) -> String {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    match resolve_sandboxed_path(working_dir, path_str) {
        Ok(path) => match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                if content.len() > MAX_READ_BYTES {
                    format!(
                        "{}...\n\n[truncated at {} bytes, file is {} bytes total]",
                        &content[..MAX_READ_BYTES],
                        MAX_READ_BYTES,
                        content.len()
                    )
                } else {
                    content
                }
            }
            Err(e) => format!("Error reading file: {e}"),
        },
        Err(e) => format!("Error: {e}"),
    }
}

async fn exec_write_file(working_dir: &Path, args: &Value) -> String {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    match resolve_sandboxed_path(working_dir, path_str) {
        Ok(path) => {
            // Create parent dirs if needed
            if let Some(parent) = path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return format!("Error creating directories: {e}");
                }
            }
            match tokio::fs::write(&path, content).await {
                Ok(()) => format!(
                    "Successfully wrote {} bytes to {}",
                    content.len(),
                    path.display()
                ),
                Err(e) => format!("Error writing file: {e}"),
            }
        }
        Err(e) => format!("Error: {e}"),
    }
}

fn bash_timeout_ms(args: &Value) -> u64 {
    args.get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
        .clamp(1, MAX_BASH_TIMEOUT_MS)
}

async fn exec_bash(
    working_dir: &Path,
    rsi_session_id: Option<uuid::Uuid>,
    launch_invocation_id: uuid::Uuid,
    cancel: &CancellationToken,
    args: &Value,
) -> String {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let timeout_ms = bash_timeout_ms(args);

    let mut command_process = tokio::process::Command::new("bash");
    command_process
        .args(["-c", command])
        .current_dir(working_dir);
    command_process.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    if let Some(session_id) = rsi_session_id {
        command_process.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
    }
    command_process.env(
        rsi_common::identity::ENV_MODEL_INVOCATION_ID,
        launch_invocation_id.to_string(),
    );

    let mut limits = CaptureLimits::local_tool();
    limits.execution_timeout = std::time::Duration::from_millis(timeout_ms);
    match capture_bounded(command_process, limits, cancel).await {
        Ok(output) => {
            let mut parts: Vec<&[u8]> = vec![&output.stdout];
            if !output.stderr.is_empty() {
                if !output.stdout.is_empty() {
                    parts.push(b"\n");
                }
                parts.push(b"[stderr]\n");
                parts.push(&output.stderr);
            }
            let result = bounded_lossy_concat(
                &parts,
                MAX_READ_BYTES,
                output.stdout_truncated || output.stderr_truncated,
                BASH_OUTPUT_TRUNCATED_MARKER,
            );
            if result.is_empty() {
                format!("[exit code: {}]", output.status.code().unwrap_or(-1))
            } else {
                result
            }
        }
        Err(CaptureError::Cancelled) => "Command cancelled".to_string(),
        Err(CaptureError::ExecutionTimedOut) => {
            format!("Command timed out after {timeout_ms}ms")
        }
        Err(error) => format!("Error executing command: {error}"),
    }
}

async fn execute_tool(
    working_dir: &Path,
    rsi_session_id: Option<uuid::Uuid>,
    launch_invocation_id: uuid::Uuid,
    cancel: &CancellationToken,
    name: &str,
    args: &Value,
) -> String {
    match name {
        "read_file" => exec_read_file(working_dir, args).await,
        "write_file" => exec_write_file(working_dir, args).await,
        "bash" => {
            exec_bash(
                working_dir,
                rsi_session_id,
                launch_invocation_id,
                cancel,
                args,
            )
            .await
        }
        _ => format!("Unknown tool: {name}"),
    }
}

// ---------------------------------------------------------------------------
// SSE streaming
// ---------------------------------------------------------------------------

/// Accumulated tool call from streaming deltas.
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Read an SSE stream from a chat completions response, emitting incremental
/// text events and accumulating tool call deltas.
///
/// Returns `(content_text, tool_calls, finish_reason)`.
async fn read_sse_stream(
    resp: reqwest::Response,
    session_tag: &str,
    event_tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    attempt_input_tokens: &mut u64,
    attempt_output_tokens: &mut u64,
) -> Result<(String, Vec<AccumulatedToolCall>, Option<String>)> {
    let mut content_text = String::new();
    let mut tool_calls: Vec<AccumulatedToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut line_buf = String::new();

    let mut byte_stream = resp.bytes_stream();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                return Ok((content_text, tool_calls, Some("cancelled".to_string())));
            }
            chunk = byte_stream.next() => chunk,
        };

        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => {
                return Err(DaemonError::Process(format!("Stream read error: {e}")));
            }
            None => break, // stream ended
        };

        // Append to line buffer and process complete lines
        let text = String::from_utf8_lossy(&chunk);
        line_buf.push_str(&text);

        while let Some(newline_pos) = line_buf.find('\n') {
            let line = line_buf[..newline_pos].trim().to_string();
            line_buf.drain(..=newline_pos);

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            let data = if let Some(stripped) = line.strip_prefix("data: ") {
                stripped.trim()
            } else {
                continue;
            };

            if data == "[DONE]" {
                break;
            }

            let chunk_json: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    tracing::trace!(error = %e, data, "Skipping unparseable SSE chunk");
                    continue;
                }
            };

            // Extract usage from the final chunk (stream_options.include_usage)
            if let Some(usage) = chunk_json.get("usage").filter(|u| !u.is_null()) {
                *attempt_input_tokens = usage
                    .get("prompt_tokens")
                    .or_else(|| usage.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                *attempt_output_tokens = usage
                    .get("completion_tokens")
                    .or_else(|| usage.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
            }

            let Some(choice) = chunk_json
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
            else {
                continue;
            };

            // Check finish_reason
            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish_reason = Some(fr.to_string());
            }

            let Some(delta) = choice.get("delta") else {
                continue;
            };

            // Accumulate content text and emit incrementally
            if let Some(text_chunk) = delta.get("content").and_then(|v| v.as_str()) {
                if !text_chunk.is_empty() {
                    content_text.push_str(text_chunk);

                    // Emit incremental text for live display
                    let _ = event_tx
                        .send(StreamEvent {
                            event_type: "content_block_delta".to_string(),
                            data: serde_json::json!({
                                "session_id": session_tag,
                                "delta": { "type": "text_delta", "text": text_chunk },
                            }),
                        })
                        .await;
                }
            }

            // Accumulate tool call deltas
            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc_delta in tcs {
                    let idx = tc_delta.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                    // Grow vector if needed
                    while tool_calls.len() <= idx {
                        tool_calls.push(AccumulatedToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });
                    }

                    if let Some(id) = tc_delta.get("id").and_then(|v| v.as_str()) {
                        tool_calls[idx].id = id.to_string();
                    }
                    if let Some(func) = tc_delta.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            tool_calls[idx].name = name.to_string();
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            tool_calls[idx].arguments.push_str(args);
                        }
                    }
                }
            }
        }
    }

    Ok((content_text, tool_calls, finish_reason))
}

// ---------------------------------------------------------------------------
// Agentic loop
// ---------------------------------------------------------------------------

/// Maximum tool-call iterations to prevent infinite loops.
const MAX_ITERATIONS: usize = 50;

async fn run_agentic_loop(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    query: &str,
    system_prompt: Option<&str>,
    working_dir: &Path,
    prior_history: Option<&[Value]>,
    rsi_session_id: Option<uuid::Uuid>,
    launch_invocation_id: uuid::Uuid,
    event_tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
) -> Result<()> {
    let url = format!("{}/chat/completions", base_url);

    // Build messages — either from prior history (resume) or fresh
    let mut messages: Vec<Value> = Vec::new();
    if let Some(history) = prior_history {
        // Resume: replay prior conversation, then append the new user query
        messages.extend_from_slice(history);
        tracing::info!(
            history_len = history.len(),
            "Resuming with conversation history"
        );
    } else if let Some(sys) = system_prompt {
        messages.push(serde_json::json!({"role": "system", "content": sys}));
    }
    messages.push(serde_json::json!({"role": "user", "content": query}));

    let tools = tool_schemas();

    // Emit init event
    let session_tag = uuid::Uuid::new_v4().to_string();
    event_tx
        .send(StreamEvent {
            event_type: "system".to_string(),
            data: serde_json::json!({
                "subtype": "init",
                "session_id": session_tag,
                "model": model,
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;

    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;

    for iteration in 0..MAX_ITERATIONS {
        if cancel.is_cancelled() {
            tracing::info!("OpenAI agentic loop cancelled");
            break;
        }

        tracing::debug!(
            iteration,
            messages_len = messages.len(),
            "Sending chat completion request"
        );

        let admitted_call = model_call_control
            .admit(
                ModelCallKind::Primary,
                &format!("iteration-{iteration}"),
                &serde_json::to_string(&messages).unwrap_or_default(),
                None,
            )
            .await?;
        let (settlement, execution) = admitted_call.into_parts();
        let started_at = std::time::Instant::now();
        let resp = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("Request cancelled");
                model_call_control.fail(settlement, "cancelled").await?;
                break;
            }
            result = send_openai_chat_request(
                http,
                &url,
                api_key,
                model,
                &messages,
                tools,
                execution,
            ) => {
                match result {
                    Ok(response) => response,
                    Err(error) => {
                        model_call_control.fail(settlement, "http_send_failed").await?;
                        return Err(error);
                    }
                }
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            model_call_control
                .fail(settlement, "http_status_failed")
                .await?;
            return Err(DaemonError::Process(format!(
                "API returned {status}: {body}"
            )));
        }

        let mut attempt_input_tokens = 0;
        let mut attempt_output_tokens = 0;
        let stream_result = read_sse_stream(
            resp,
            &session_tag,
            event_tx,
            cancel,
            &mut attempt_input_tokens,
            &mut attempt_output_tokens,
        )
        .await;
        let (content_text, accumulated_tool_calls, finish_reason) = match stream_result {
            Ok(result) => result,
            Err(error) => {
                model_call_control.fail(settlement, "stream_failed").await?;
                return Err(error);
            }
        };
        if finish_reason.as_deref() == Some("cancelled") {
            model_call_control.fail(settlement, "cancelled").await?;
            break;
        }
        model_call_control
            .complete(
                settlement,
                ModelCallUsage {
                    input_tokens: Some(attempt_input_tokens),
                    output_tokens: Some(attempt_output_tokens),
                    wall_time_ms: Some(
                        started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
                    ),
                    confidence: Some(rsi_common::model_control::ModelUsageConfidence::Measured),
                    ..ModelCallUsage::default()
                },
            )
            .await?;
        total_input_tokens = total_input_tokens.saturating_add(attempt_input_tokens);
        total_output_tokens = total_output_tokens.saturating_add(attempt_output_tokens);

        // Emit final assistant text event (for the complete message, used by event persistence)
        if !content_text.is_empty() {
            event_tx
                .send(StreamEvent {
                    event_type: "assistant".to_string(),
                    data: serde_json::json!({
                        "session_id": session_tag,
                        "message": {
                            "role": "assistant",
                            "content": [{"type": "text", "text": content_text}],
                        },
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;
        }

        if finish_reason.as_deref() != Some("tool_calls") {
            // Session complete — emit result
            event_tx
                .send(StreamEvent {
                    event_type: "result".to_string(),
                    data: serde_json::json!({
                        "session_id": session_tag,
                        "subtype": "turn_completed",
                        "usage": {
                            "input_tokens": total_input_tokens,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "output_tokens": total_output_tokens,
                        },
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;
            return Ok(());
        }

        // Handle tool calls from accumulated deltas
        if accumulated_tool_calls.is_empty() {
            tracing::warn!("finish_reason=tool_calls but no tool_calls accumulated");
            break;
        }

        // Build the assistant message with tool_calls for conversation history
        let tc_values: Vec<Value> = accumulated_tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments,
                    },
                })
            })
            .collect();

        let mut assistant_msg = serde_json::json!({
            "role": "assistant",
            "tool_calls": tc_values,
        });
        if !content_text.is_empty() {
            assistant_msg["content"] = Value::String(content_text.clone());
        }
        messages.push(assistant_msg);

        for tc in &accumulated_tool_calls {
            let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));

            // Emit tool_use event
            event_tx
                .send(StreamEvent {
                    event_type: "tool_use".to_string(),
                    data: serde_json::json!({
                        "session_id": session_tag,
                        "name": tc.name,
                        "input": args,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            // Execute tool
            let result = execute_tool(
                working_dir,
                rsi_session_id,
                launch_invocation_id,
                cancel,
                &tc.name,
                &args,
            )
            .await;

            // Emit tool_result event
            event_tx
                .send(StreamEvent {
                    event_type: "tool_result".to_string(),
                    data: serde_json::json!({
                        "session_id": session_tag,
                        "content": result,
                        "name": tc.name,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            // Append tool result to messages
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tc.id,
                "name": tc.name,
                "content": result,
            }));
        }
    }

    // If we hit MAX_ITERATIONS, emit a final result
    event_tx
        .send(StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "session_id": session_tag,
                "subtype": "turn_completed",
                "usage": {
                    "input_tokens": total_input_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": total_output_tokens,
                },
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;

    Ok(())
}

async fn send_openai_chat_request(
    http: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    model: &str,
    messages: &[Value],
    tools: &Value,
    execution: ModelExecutionCapability,
) -> Result<reqwest::Response> {
    let mut request = http.post(url).json(&serde_json::json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true },
    }));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    execution
        .bind_http(
            crate::model_control::registry::RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            request,
        )
        .send("OpenAI compatible")
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::model_control::call_control::{
        AdmittedModelCall, ModelCallKind, ModelCallSettlement, ModelCallSettlementWorker,
        StoreBackedModelCallControl,
    };
    use crate::model_control::registry::RuntimeExecutionRoute;
    use crate::model_control::{
        AdmissionDecision, ModelAdmissionRequest, admit_invocation, explicit_expected_usage,
    };
    use crate::store::Store;
    use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose};
    use rusqlite::params;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex, oneshot};

    #[test]
    fn bash_timeout_is_nonzero_and_bounded() {
        assert_eq!(bash_timeout_ms(&serde_json::json!({})), 120_000);
        assert_eq!(bash_timeout_ms(&serde_json::json!({"timeout_ms": 0})), 1);
        assert_eq!(
            bash_timeout_ms(&serde_json::json!({"timeout_ms": 120_001})),
            120_000
        );
    }

    #[tokio::test]
    async fn bash_tool_uses_bound_rsi_ownership_ids() {
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let cancel = CancellationToken::new();
        let output = exec_bash(
            &std::env::temp_dir(),
            Some(session_id),
            invocation_id,
            &cancel,
            &serde_json::json!({
                "command": format!(
                    "printf '%s\\n%s' \"${{{}:-MISSING}}\" \"${{{}:-MISSING}}\"",
                    rsi_common::identity::ENV_SESSION_ID,
                    rsi_common::identity::ENV_MODEL_INVOCATION_ID,
                )
            }),
        )
        .await;

        assert_eq!(output, format!("{session_id}\n{invocation_id}"));
    }

    #[cfg(target_os = "linux")]
    async fn read_spawned_pid(path: &Path) -> u32 {
        for _ in 0..200 {
            if let Ok(raw) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = raw.parse()
            {
                return pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for subprocess pid at {}", path.display());
    }

    #[cfg(target_os = "linux")]
    async fn assert_process_reaped(pid: u32) {
        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..200 {
            if !proc_path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("subprocess {pid} remained after tool completion");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_timeout_kills_and_reaps_direct_child() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let pid_path = temp.path().join("bash.pid");
        let cancel = CancellationToken::new();
        let output = exec_bash(
            temp.path(),
            Some(uuid::Uuid::new_v4()),
            uuid::Uuid::new_v4(),
            &cancel,
            &serde_json::json!({
                "command": format!(
                    "printf %d $$ > {}; exec sleep 10",
                    pid_path.display()
                ),
                "timeout_ms": 100
            }),
        )
        .await;

        assert_eq!(output, "Command timed out after 100ms");
        let pid = read_spawned_pid(&pid_path).await;
        assert_process_reaped(pid).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_cancellation_kills_and_reaps_direct_child() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let pid_path = temp.path().join("bash.pid");
        let working_dir = temp.path().to_path_buf();
        let args = serde_json::json!({
            "command": format!(
                "printf %d $$ > {}; exec sleep 10",
                pid_path.display()
            ),
            "timeout_ms": 5_000
        });
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            exec_bash(
                &working_dir,
                Some(uuid::Uuid::new_v4()),
                uuid::Uuid::new_v4(),
                &task_cancel,
                &args,
            )
            .await
        });

        let pid = read_spawned_pid(&pid_path).await;
        cancel.cancel();
        assert_eq!(task.await.expect("bash task"), "Command cancelled");
        assert_process_reaped(pid).await;
    }

    struct TestControl {
        control: Arc<StoreBackedModelCallControl>,
        worker: ModelCallSettlementWorker,
        store: Arc<Mutex<Store>>,
    }

    async fn test_control(provider: &str) -> TestControl {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(32));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some(provider.to_string()),
            model: Some("test-model".to_string()),
            backend: Some("openai_compatible_http".to_string()),
            effort: None,
            trigger: "openai_production_path_test".to_string(),
            owner: InvocationOwner {
                session_id: Some(session_id),
                ..InvocationOwner::default()
            },
            dedup_key: Some(format!("openai-root:{session_id}")),
            request_fingerprint: Some("sha256:openai-root".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::SessionLaunchFresh,
                Some(provider),
                Some("openai_compatible_http"),
                Some("test-model"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let permit = match admit_invocation(&store, request, &event_bus)
            .await
            .expect("root admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate root admission: {invocation_id}")
            }
        };
        let control = Arc::new(StoreBackedModelCallControl::new(
            Arc::clone(&store),
            event_bus,
            worker.handle().expect("settlement handle"),
            InvocationOwner {
                session_id: Some(session_id),
                ..InvocationOwner::default()
            },
            provider,
            Some("test-model".to_string()),
            "openai_compatible_http",
            None,
            "openai_production_path_test",
            permit,
            ModelInvocationPurpose::SessionOpenAiCompatibleTurn,
            None,
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
        ));
        TestControl {
            control,
            worker,
            store,
        }
    }

    fn sse_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    async fn loopback_server(
        response_bodies: Vec<String>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            for body in response_bodies {
                let (mut socket, _) = listener.accept().await.expect("request connection");
                let mut request = vec![0_u8; 16 * 1024];
                let bytes = socket.read(&mut request).await.expect("request bytes");
                let request = std::str::from_utf8(&request[..bytes]).expect("UTF-8 request");
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                server_requests.fetch_add(1, Ordering::SeqCst);
                socket
                    .write_all(&sse_response(&body))
                    .await
                    .expect("SSE response");
            }
        });
        (format!("http://{address}/v1"), requests, server)
    }

    fn final_sse(text: &str, input: u64, output: u64) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output}}}}}\n\ndata: [DONE]\n\n"
        )
    }

    fn tool_sse() -> String {
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"unknown_test_tool\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n".to_string()
    }

    async fn run_test_loop(
        base_url: &str,
        control: Arc<dyn ModelCallControl>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let (event_tx, _event_rx) = mpsc::channel(100);
        run_agentic_loop(
            &reqwest::Client::new(),
            base_url,
            None,
            "test-model",
            "hello",
            None,
            &std::env::temp_dir(),
            None,
            None,
            uuid::Uuid::new_v4(),
            &event_tx,
            &cancel,
            control,
        )
        .await
    }

    async fn invocation_states(store: &Arc<Mutex<Store>>) -> Vec<(String, Option<String>)> {
        let store = store.lock().await;
        let mut statement = store
            .conn
            .prepare("SELECT status, error_class FROM model_invocations ORDER BY created_at")
            .expect("invocation query");
        statement
            .query_map(params![], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("invocation rows")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect invocation rows")
    }

    #[derive(Default)]
    struct DenyingControl;

    #[async_trait::async_trait]
    impl ModelCallControl for DenyingControl {
        async fn admit(
            &self,
            _kind: ModelCallKind,
            _dedup_hint: &str,
            _fingerprint_hint: &str,
            _retry_of_invocation_id: Option<uuid::Uuid>,
        ) -> Result<AdmittedModelCall> {
            Err(DaemonError::PolicyDenied("test denial".to_string()))
        }

        async fn complete(&self, _call: ModelCallSettlement, _usage: ModelCallUsage) -> Result<()> {
            unreachable!("denied calls have no settlement")
        }

        async fn fail(&self, _call: ModelCallSettlement, _error_class: &str) -> Result<()> {
            unreachable!("denied calls have no settlement")
        }

        async fn fail_pending(&self, _error_class: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_tool_schemas_valid_json() {
        let schemas = tool_schemas();
        let tools = schemas.as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["function"]["name"].as_str().unwrap(), "read_file");
        assert_eq!(tools[1]["function"]["name"].as_str().unwrap(), "write_file");
        assert_eq!(tools[2]["function"]["name"].as_str().unwrap(), "bash");
    }

    #[test]
    fn test_resolve_sandboxed_path_normal() {
        let wd = std::env::temp_dir();
        let result = resolve_sandboxed_path(&wd, "foo/bar.txt");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(wd.canonicalize().unwrap()));
    }

    #[test]
    fn test_resolve_sandboxed_path_traversal_rejected() {
        let wd = std::env::temp_dir().join("test_sandbox");
        std::fs::create_dir_all(&wd).ok();
        let result = resolve_sandboxed_path(&wd, "../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_sandboxed_path_absolute_rejected() {
        let wd = std::env::temp_dir();
        let result = resolve_sandboxed_path(&wd, "/etc/passwd");
        // Absolute paths outside working_dir should be rejected
        if let Ok(path) = result {
            assert!(path.starts_with(wd.canonicalize().unwrap()));
        }
    }

    #[tokio::test]
    async fn denied_openai_compatible_turn_sends_no_request() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let result = run_test_loop(
            &format!("http://{address}/v1"),
            Arc::new(DenyingControl),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(DaemonError::PolicyDenied(_))));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "policy denial must occur before transport"
        );
    }

    #[tokio::test]
    async fn one_openai_compatible_iteration_has_one_request_and_terminal_row() {
        let (base_url, requests, server) = loopback_server(vec![final_sse("done", 3, 2)]).await;
        let test = test_control("Local").await;

        run_test_loop(
            &base_url,
            Arc::clone(&test.control) as Arc<dyn ModelCallControl>,
            CancellationToken::new(),
        )
        .await
        .expect("one admitted iteration");
        server.await.expect("loopback server");

        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            invocation_states(&test.store).await,
            vec![("completed".to_string(), None)]
        );
        drop(test.control);
        test.worker.shutdown().await.expect("settlement shutdown");
    }

    #[tokio::test]
    async fn tool_loop_iterations_are_independently_admitted_and_settled() {
        let (base_url, requests, server) =
            loopback_server(vec![tool_sse(), final_sse("done", 5, 4)]).await;
        let test = test_control("Local").await;

        run_test_loop(
            &base_url,
            Arc::clone(&test.control) as Arc<dyn ModelCallControl>,
            CancellationToken::new(),
        )
        .await
        .expect("two admitted iterations");
        server.await.expect("loopback server");

        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            invocation_states(&test.store).await,
            vec![
                ("completed".to_string(), None),
                ("completed".to_string(), None),
            ]
        );
        drop(test.control);
        test.worker.shutdown().await.expect("settlement shutdown");
    }

    #[tokio::test]
    async fn cancellation_after_admission_settles_once() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request connection");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("request bytes");
            let _ = accepted_tx.send(());
            std::future::pending::<()>().await;
        });
        let test = test_control("Local").await;
        let cancel = CancellationToken::new();
        let loop_cancel = cancel.clone();
        let control = Arc::clone(&test.control) as Arc<dyn ModelCallControl>;
        let task = tokio::spawn(async move {
            run_test_loop(&format!("http://{address}/v1"), control, loop_cancel).await
        });
        accepted_rx.await.expect("request accepted");
        cancel.cancel();
        task.await
            .expect("loop task")
            .expect("cancelled loop exits");
        server.abort();

        assert_eq!(
            invocation_states(&test.store).await,
            vec![("failed".to_string(), Some("cancelled".to_string()))]
        );
        drop(test.control);
        test.worker.shutdown().await.expect("settlement shutdown");
    }

    #[test]
    fn compatible_provider_label_follows_resolved_transport() {
        let loopback = OpenAiClient::with_config("http://127.0.0.1:11434/v1".to_string(), None)
            .expect("loopback client");
        let remote = OpenAiClient::with_config("https://models.example.test/v1".to_string(), None)
            .expect("remote client");
        assert_eq!(loopback.resolved_provider_label(), "Local");
        assert_eq!(remote.resolved_provider_label(), "OpenAICompatible");
    }
}
