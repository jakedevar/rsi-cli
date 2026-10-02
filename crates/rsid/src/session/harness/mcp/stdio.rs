use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::{Child, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use crate::process_control::{
    ProcessContainment, configure_tokio_process_group, terminate_process_group,
};

const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_TOOLS: usize = 256;
const DEFAULT_MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct McpServerSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env_allowlist_pairs: BTreeMap<String, String>,
    pub working_dir: Option<PathBuf>,
}

impl fmt::Debug for McpServerSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerSpec")
            .field("command", &self.command)
            .field("args", &self.args)
            .field(
                "env_names",
                &self.env_allowlist_pairs.keys().collect::<Vec<_>>(),
            )
            .field("env_values", &"<redacted>")
            .field("working_dir", &self.working_dir)
            .finish()
    }
}

impl McpServerSpec {
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env_allowlist_pairs: BTreeMap::new(),
            working_dir: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env_pair(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_allowlist_pairs.insert(key.into(), value.into());
        self
    }

    pub fn working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(path.into());
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct McpLimits {
    pub initialize_timeout: Duration,
    pub call_timeout: Duration,
    pub cleanup_timeout: Duration,
    pub max_tools: usize,
    pub max_description_bytes: usize,
    pub max_response_bytes: usize,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            initialize_timeout: DEFAULT_INITIALIZE_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
            max_tools: DEFAULT_MAX_TOOLS,
            max_description_bytes: DEFAULT_MAX_DESCRIPTION_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpToolCallOutput {
    pub is_error: bool,
    pub text: String,
    pub structured_content: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpError {
    InvalidCommand,
    Spawn,
    InitializeTimeout,
    CallTimeout,
    Cancelled,
    CleanupTimeout,
    ResponseTooLarge,
    ToolCountExceeded,
    DescriptionTooLarge,
    OutputTooLarge,
    Protocol,
}

impl std::fmt::Display for McpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCommand => formatter.write_str("MCP command must be an absolute path"),
            Self::Spawn => formatter.write_str("MCP server process could not be spawned"),
            Self::InitializeTimeout => formatter.write_str("MCP initialization timed out"),
            Self::CallTimeout => formatter.write_str("MCP tool call timed out"),
            Self::Cancelled => formatter.write_str("MCP tool call was cancelled"),
            Self::CleanupTimeout => formatter.write_str("MCP server cleanup timed out"),
            Self::ResponseTooLarge => formatter.write_str("MCP response exceeded its byte limit"),
            Self::ToolCountExceeded => formatter.write_str("MCP tool count exceeded its limit"),
            Self::DescriptionTooLarge => {
                formatter.write_str("MCP tool description exceeded its byte limit")
            }
            Self::OutputTooLarge => formatter.write_str("MCP tool output exceeded its byte limit"),
            Self::Protocol => formatter.write_str("MCP protocol error"),
        }
    }
}

impl std::error::Error for McpError {}

#[derive(Debug)]
struct BoundedChildStdout {
    inner: ChildStdout,
    budget: Arc<ResponseBudget>,
}

impl AsyncRead for BoundedChildStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let filled_before = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                let bytes_read = buffer.filled().len().saturating_sub(filled_before);
                let prior = this.budget.bytes.load(Ordering::Acquire);
                if prior.saturating_add(bytes_read) > this.budget.max_bytes {
                    this.budget.overflow.store(true, Ordering::Release);
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "MCP response exceeded its byte limit",
                    )));
                }
                this.budget.bytes.fetch_add(bytes_read, Ordering::Release);
                Poll::Ready(Ok(()))
            }
            poll => poll,
        }
    }
}

#[derive(Debug, Default)]
struct ResponseBudget {
    bytes: AtomicUsize,
    overflow: AtomicBool,
    max_bytes: usize,
}

impl ResponseBudget {
    fn new(max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            bytes: AtomicUsize::new(0),
            overflow: AtomicBool::new(false),
            max_bytes,
        })
    }

    fn reset(&self) {
        self.bytes.store(0, Ordering::Release);
        self.overflow.store(false, Ordering::Release);
    }

    fn overflowed(&self) -> bool {
        self.overflow.load(Ordering::Acquire)
    }
}

pub struct StdioMcpClient {
    child: Option<Child>,
    client: Option<RunningService<RoleClient, ()>>,
    env_names: Vec<String>,
    env_values: Vec<String>,
    budget: Arc<ResponseBudget>,
    limits: McpLimits,
}

impl fmt::Debug for StdioMcpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StdioMcpClient")
            .field("child_pid", &self.child.as_ref().and_then(Child::id))
            .field("has_client", &self.client.is_some())
            .field("env_names", &self.env_names)
            .field("env_values", &"<redacted>")
            .field("budget", &self.budget)
            .field("limits", &self.limits)
            .finish()
    }
}

impl StdioMcpClient {
    pub async fn connect(spec: &McpServerSpec, limits: McpLimits) -> Result<Self, McpError> {
        if !spec.command.is_absolute() {
            return Err(McpError::InvalidCommand);
        }

        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(working_dir) = &spec.working_dir {
            command.current_dir(working_dir);
        }
        for (key, value) in &spec.env_allowlist_pairs {
            command.env(key, value);
        }
        configure_tokio_process_group(&mut command, ProcessContainment::GroupNoEscape)
            .map_err(|_| McpError::Spawn)?;
        command.kill_on_drop(true);

        let mut child = command.spawn().map_err(|_| McpError::Spawn)?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.start_kill();
                return Err(McpError::Spawn);
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.start_kill();
                return Err(McpError::Spawn);
            }
        };
        let budget = ResponseBudget::new(limits.max_response_bytes);
        let bounded_stdout = BoundedChildStdout {
            inner: stdout,
            budget: Arc::clone(&budget),
        };

        let connected = match tokio::time::timeout(
            limits.initialize_timeout,
            ().serve((bounded_stdout, stdin)),
        )
        .await
        {
            Err(_) => {
                cleanup_child(&mut child, limits.cleanup_timeout).await;
                return Err(McpError::InitializeTimeout);
            }
            Ok(Err(_)) => {
                cleanup_child(&mut child, limits.cleanup_timeout).await;
                return Err(if budget.overflowed() {
                    McpError::ResponseTooLarge
                } else {
                    McpError::Protocol
                });
            }
            Ok(Ok(client)) => client,
        };

        Ok(Self {
            child: Some(child),
            client: Some(connected),
            env_names: spec.env_allowlist_pairs.keys().cloned().collect(),
            env_values: spec.env_allowlist_pairs.values().cloned().collect(),
            budget,
            limits,
        })
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, McpError> {
        let cancellation = CancellationToken::new();
        self.list_tools_with_cancellation(&cancellation).await
    }

    pub async fn list_tools_with_cancellation(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<McpToolInfo>, McpError> {
        self.budget.reset();
        let client = match self.client.as_mut() {
            Some(client) => client,
            None => return Err(McpError::Protocol),
        };
        let result = tokio::select! {
            result = client.list_all_tools() => Some(result),
            _ = tokio::time::sleep(self.limits.call_timeout) => None,
            _ = cancellation.cancelled() => None,
        };

        match result {
            Some(Ok(tools)) if tools.len() > self.limits.max_tools => {
                self.terminate().await;
                Err(McpError::ToolCountExceeded)
            }
            Some(Ok(_tools)) if self.budget.overflowed() => {
                self.terminate().await;
                Err(McpError::ResponseTooLarge)
            }
            Some(Ok(tools)) => {
                let mut mapped = Vec::with_capacity(tools.len());
                for tool in tools {
                    let description = tool.description.as_deref();
                    if description.is_some_and(|description| {
                        description.as_bytes().len() > self.limits.max_description_bytes
                    }) {
                        self.terminate().await;
                        return Err(McpError::DescriptionTooLarge);
                    }
                    mapped.push(McpToolInfo {
                        name: self.redact(tool.name.to_string()),
                        description: description
                            .map(|description| self.redact(description.to_string())),
                        input_schema: Some(
                            self.redact_json(Value::Object(tool.input_schema.as_ref().clone())),
                        ),
                    });
                }
                Ok(mapped)
            }
            Some(Err(_)) => {
                self.terminate().await;
                if self.budget.overflowed() {
                    Err(McpError::ResponseTooLarge)
                } else {
                    Err(McpError::Protocol)
                }
            }
            None if cancellation.is_cancelled() => {
                self.terminate().await;
                Err(McpError::Cancelled)
            }
            None => {
                self.terminate().await;
                Err(McpError::CallTimeout)
            }
        }
    }

    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<McpToolCallOutput, McpError> {
        let cancellation = CancellationToken::new();
        self.call_tool_with_cancellation(name, arguments, &cancellation)
            .await
    }

    pub async fn call_tool_with_cancellation(
        &mut self,
        name: &str,
        arguments: Option<Value>,
        cancellation: &CancellationToken,
    ) -> Result<McpToolCallOutput, McpError> {
        self.budget.reset();
        let client = match self.client.as_mut() {
            Some(client) => client,
            None => return Err(McpError::Protocol),
        };
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(arguments) = arguments {
            let Some(object) = arguments.as_object() else {
                return Err(McpError::Protocol);
            };
            params = params.with_arguments(object.clone());
        }

        let result = tokio::select! {
            result = client.call_tool(params) => Some(result),
            _ = tokio::time::sleep(self.limits.call_timeout) => None,
            _ = cancellation.cancelled() => None,
        };
        match result {
            Some(Ok(result)) => {
                let mut text = String::new();
                for content in result.content {
                    match content {
                        ContentBlock::Text(text_content) => text.push_str(&text_content.text),
                        other => {
                            let encoded =
                                serde_json::to_string(&other).map_err(|_| McpError::Protocol)?;
                            text.push_str(&encoded);
                        }
                    }
                }
                let structured_content = result
                    .structured_content
                    .map(|value| self.redact_json(value));
                let output = McpToolCallOutput {
                    is_error: result.is_error.unwrap_or(false),
                    text: self.redact(text),
                    structured_content,
                };
                let encoded = serde_json::to_vec(&output).map_err(|_| McpError::Protocol)?;
                if encoded.len() > self.limits.max_response_bytes {
                    self.terminate().await;
                    return Err(McpError::OutputTooLarge);
                }
                Ok(output)
            }
            Some(Err(_)) => {
                self.terminate().await;
                if self.budget.overflowed() {
                    Err(McpError::ResponseTooLarge)
                } else {
                    Err(McpError::Protocol)
                }
            }
            None if cancellation.is_cancelled() => {
                self.terminate().await;
                Err(McpError::Cancelled)
            }
            None => {
                self.terminate().await;
                Err(McpError::CallTimeout)
            }
        }
    }

    pub async fn shutdown(mut self) -> Result<(), McpError> {
        if self.terminate().await {
            Ok(())
        } else {
            Err(McpError::CleanupTimeout)
        }
    }

    pub async fn terminate(&mut self) -> bool {
        if let Some(child) = self.child.as_mut() {
            terminate_child_group(child);
        }
        let client = match self.client.as_mut() {
            Some(client) => client,
            None => return false,
        };
        cleanup_service(client, self.child.take(), self.limits.cleanup_timeout).await
    }

    fn redact(&self, value: impl AsRef<str>) -> String {
        // Defense-in-depth only: the child already receives the secret and may re-encode it.
        let mut value = value.as_ref().to_string();
        for secret in &self.env_values {
            if !secret.is_empty() {
                value = value.replace(secret, "<redacted>");
            }
        }
        value
    }

    fn redact_json(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(text)),
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| self.redact_json(item))
                    .collect(),
            ),
            Value::Object(fields) => {
                let mut object = serde_json::Map::new();
                // Next free suffix per redacted base: hostile key collisions
                // stay linear instead of rescanning from #1 for every key.
                let mut next_suffix: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for (key, value) in fields {
                    let base = self.redact(key);
                    let mut redacted_key = base.clone();
                    if object.contains_key(&redacted_key) {
                        let suffix = next_suffix.entry(base.clone()).or_insert(1);
                        loop {
                            redacted_key = format!("{base}#{suffix}");
                            *suffix += 1;
                            if !object.contains_key(&redacted_key) {
                                break;
                            }
                        }
                    }
                    object.insert(redacted_key, self.redact_json(value));
                }
                Value::Object(object)
            }
            value => value,
        }
    }
}

impl Drop for StdioMcpClient {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            terminate_child_group(child);
        }
        let child = self.child.take();
        let Some(mut client) = self.client.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let timeout = self.limits.cleanup_timeout;
            handle.spawn(async move {
                let _ = cleanup_service(&mut client, child, timeout).await;
            });
        } else {
            drop(client);
            if let Some(mut child) = child {
                let _ = child.start_kill();
            }
        }
    }
}

fn terminate_child_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        terminate_process_group(nix::unistd::Pid::from_raw(pid as i32));
    }
}

async fn cleanup_service(
    client: &mut RunningService<RoleClient, ()>,
    mut child: Option<Child>,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        let _ = client.close().await;
        if let Some(child) = child.as_mut() {
            let _ = child.wait().await;
        }
    })
    .await
    .is_ok()
}

async fn cleanup_child(child: &mut Child, timeout: Duration) -> bool {
    terminate_child_group(child);
    tokio::time::timeout(timeout, child.wait())
        .await
        .map(|result| result.is_ok())
        .unwrap_or(false)
}
