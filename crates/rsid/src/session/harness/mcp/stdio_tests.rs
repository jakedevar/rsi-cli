use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{McpError, McpLimits, McpServerSpec, StdioMcpClient};

const TEST_SERVER: &str = r#"
import json
import os
import subprocess
import sys
import time

mode = os.environ["MCP_TEST_MODE"]
pid_file = os.environ["MCP_TEST_PID_FILE"]

with open(pid_file, "w") as file:
    file.write(str(os.getpid()))

if mode == "group":
    child = subprocess.Popen(["/bin/sleep", "30"])
    with open(os.environ["MCP_TEST_GROUP_PID_FILE"], "w") as file:
        file.write(str(child.pid))

if mode == "hang_initialize":
    while True:
        time.sleep(1)

def send(identifier, result):
    sys.stdout.write(json.dumps({
        "jsonrpc": "2.0",
        "id": identifier,
        "result": result,
    }, separators=(",", ":")) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    identifier = message.get("id")
    if method == "initialize":
        send(identifier, {
            "protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "rsi-test", "version": "1.0"},
        })
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        if mode == "many_tools":
            tools = [{
                "name": f"tool-{index}",
                "inputSchema": {"type": "object"},
            } for index in range(3)]
        elif mode == "long_description":
            tools = [{"name": "long", "description": "x" * 100, "inputSchema": {}}]
        else:
            tools = [{
                "name": "echo",
                "description": "Echo the input",
                "inputSchema": {"type": "object"},
            }]
        send(identifier, {"tools": tools})
    elif method == "tools/call":
        if mode == "hang_call":
            while True:
                time.sleep(1)
        if mode == "oversized":
            text = "x" * 5000
        elif mode == "echo_env":
            text = json.dumps(dict(os.environ), sort_keys=True)
        else:
            text = "called"
        send(identifier, {
            "content": [{"type": "text", "text": text}],
            "structuredContent": {"result": text},
            "isError": False,
        })
"#;

fn spec(mode: &str, temp: &TempDir) -> McpServerSpec {
    let pid_file = temp.path().join(format!("{mode}.pid"));
    McpServerSpec::new("/usr/bin/python3")
        .arg("-c")
        .arg(TEST_SERVER)
        .env_pair("MCP_TEST_MODE", mode)
        .env_pair("MCP_TEST_PID_FILE", pid_file.to_string_lossy().as_ref())
}

fn quick_limits() -> McpLimits {
    McpLimits {
        initialize_timeout: Duration::from_secs(2),
        call_timeout: Duration::from_secs(1),
        cleanup_timeout: Duration::from_secs(1),
        max_tools: 256,
        max_description_bytes: 16 * 1024,
        max_response_bytes: 1024 * 1024,
    }
}

fn read_pid(temp: &TempDir, mode: &str) -> i32 {
    std::fs::read_to_string(temp.path().join(format!("{mode}.pid")))
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid")
}

fn process_alive(pid: i32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn stdio_client_lists_and_calls_tools() {
    let temp = TempDir::new().expect("temp dir");
    let mut client = StdioMcpClient::connect(&spec("basic", &temp), quick_limits())
        .await
        .expect("connect");

    let tools = client.list_tools().await.expect("list tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description.as_deref(), Some("Echo the input"));

    let output = client
        .call_tool("echo", Some(json!({"value": "hello"})))
        .await
        .expect("call tool");
    assert!(!output.is_error);
    assert_eq!(output.text, "called");
    assert_eq!(output.structured_content, Some(json!({"result": "called"})));

    client.shutdown().await.expect("shutdown");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn initialization_timeout_kills_the_server() {
    let temp = TempDir::new().expect("temp dir");
    let mut limits = quick_limits();
    limits.initialize_timeout = Duration::from_millis(100);

    let error = StdioMcpClient::connect(&spec("hang_initialize", &temp), limits)
        .await
        .expect_err("initialize timeout");
    assert_eq!(error, McpError::InitializeTimeout);

    let pid = read_pid(&temp, "hang_initialize");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!process_alive(pid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn call_timeout_kills_the_server() {
    let temp = TempDir::new().expect("temp dir");
    let mut client = StdioMcpClient::connect(&spec("hang_call", &temp), quick_limits())
        .await
        .expect("connect");

    let error = client
        .call_tool("echo", Some(json!({"value": "hello"})))
        .await
        .expect_err("call timeout");
    assert_eq!(error, McpError::CallTimeout);

    let pid = read_pid(&temp, "hang_call");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!process_alive(pid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn cancellation_kills_the_server() {
    let temp = TempDir::new().expect("temp dir");
    let mut client = StdioMcpClient::connect(&spec("hang_call", &temp), quick_limits())
        .await
        .expect("connect");
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        client
            .call_tool_with_cancellation("echo", None, &task_cancellation)
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancellation.cancel();
    assert_eq!(task.await.expect("task"), Err(McpError::Cancelled));

    let pid = read_pid(&temp, "hang_call");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!process_alive(pid));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn oversized_response_is_a_typed_error() {
    let temp = TempDir::new().expect("temp dir");
    let mut limits = quick_limits();
    limits.max_response_bytes = 2048;
    let mut client = StdioMcpClient::connect(&spec("oversized", &temp), limits)
        .await
        .expect("connect");

    let error = client.call_tool("echo", None).await.expect_err("oversized");
    assert_eq!(error, McpError::ResponseTooLarge);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn child_environment_is_scrubbed_and_values_are_redacted() {
    temp_env::async_with_vars(
        [("RSI_TEST_SECRET_SENTINEL", Some("parent-secret-sentinel"))],
        async {
            let temp = TempDir::new().expect("temp dir");
            let test_spec = spec("echo_env", &temp).env_pair("MCP_TEST_TOKEN", "allowed-secret");
            let mut client = StdioMcpClient::connect(&test_spec, quick_limits())
                .await
                .expect("connect");
            let output = client.call_tool("echo", None).await.expect("call");

            assert!(!output.text.contains("allowed-secret"));
            assert!(!output.text.contains("parent-secret-sentinel"));
            assert!(output.text.contains("\"MCP_TEST_TOKEN\": \"<redacted>\""));
            let structured = output
                .structured_content
                .expect("structured content")
                .to_string();
            assert!(structured.contains("MCP_TEST_TOKEN"));
            assert!(structured.contains("<redacted>"));
            assert!(!structured.contains("allowed-secret"));

            for error in [
                McpError::InvalidCommand,
                McpError::Spawn,
                McpError::InitializeTimeout,
                McpError::CallTimeout,
                McpError::Cancelled,
                McpError::CleanupTimeout,
                McpError::ResponseTooLarge,
                McpError::ToolCountExceeded,
                McpError::DescriptionTooLarge,
                McpError::OutputTooLarge,
                McpError::Protocol,
            ] {
                let text = error.to_string();
                assert!(!text.contains("allowed-secret"));
                assert!(!text.contains("parent-secret-sentinel"));
            }
        },
    )
    .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn debug_output_hides_environment_values() {
    let temp = TempDir::new().expect("temp dir");
    let test_spec = spec("basic", &temp).env_pair("MCP_TEST_TOKEN", "allowed-secret-canary");
    let spec_debug = format!("{test_spec:?}");
    let mut client = StdioMcpClient::connect(&test_spec, quick_limits())
        .await
        .expect("connect");
    let client_debug = format!("{client:?}");

    for text in [&spec_debug, &client_debug] {
        assert!(text.contains("MCP_TEST_TOKEN"));
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("allowed-secret-canary"));
    }

    client.shutdown().await.expect("shutdown");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn tool_count_and_description_limits_are_enforced() {
    let temp = TempDir::new().expect("temp dir");
    let mut count_limits = quick_limits();
    count_limits.max_tools = 1;
    let mut client = StdioMcpClient::connect(&spec("many_tools", &temp), count_limits)
        .await
        .expect("connect");
    assert_eq!(client.list_tools().await, Err(McpError::ToolCountExceeded));

    let mut description_limits = quick_limits();
    description_limits.max_description_bytes = 8;
    let mut client = StdioMcpClient::connect(&spec("long_description", &temp), description_limits)
        .await
        .expect("connect");
    assert_eq!(
        client.list_tools().await,
        Err(McpError::DescriptionTooLarge)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn drop_kills_and_reaps_the_process_group() {
    let temp = TempDir::new().expect("temp dir");
    let group_pid_file = temp.path().join("group-child.pid");
    let group_spec = spec("group", &temp).env_pair(
        "MCP_TEST_GROUP_PID_FILE",
        group_pid_file.to_string_lossy().as_ref(),
    );
    let mut client = StdioMcpClient::connect(&group_spec, quick_limits())
        .await
        .expect("connect");
    assert_eq!(client.list_tools().await.expect("tools").len(), 1);

    let server_pid = read_pid(&temp, "group");
    let group_pid = std::fs::read_to_string(group_pid_file)
        .expect("group pid")
        .trim()
        .parse::<i32>()
        .expect("group pid");
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!process_alive(server_pid));
    assert!(!process_alive(group_pid));
}
