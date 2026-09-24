use rsi_common::rpc::{RpcRequest, RpcResponse};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Wait for a socket file to appear.
#[allow(dead_code)]
async fn wait_for_socket(socket_path: &PathBuf, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if socket_path.exists() {
            // Give the daemon a moment to start accepting connections
            tokio::time::sleep(Duration::from_millis(100)).await;
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Send an RPC request and get the response.
#[allow(dead_code)]
async fn send_rpc(stream: &mut UnixStream, request: &RpcRequest) -> RpcResponse {
    let json = serde_json::to_string(request).unwrap();
    stream
        .write_all(format!("{}\n", json).as_bytes())
        .await
        .unwrap();

    let mut reader = BufReader::new(&mut *stream);
    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();

    serde_json::from_str(&response).unwrap()
}

#[test]
fn test_rpc_request_format() {
    // Verify our RPC request format is valid
    let request = RpcRequest::new("ListSessions", serde_json::Value::Null);

    assert_eq!(request.jsonrpc, "2.0");
    assert_eq!(request.method, "ListSessions");
    assert!(request.id.is_some());
}

#[test]
fn test_launch_session_params_format() {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "LaunchSession",
        "params": {
            "query": "Hello, Claude!",
            "tags": ["ci"]
        }
    });

    let parsed: RpcRequest = serde_json::from_value(request).expect("Valid request");

    let params: rsi_common::rpc::LaunchSessionParams =
        serde_json::from_value(parsed.params).expect("Valid params");

    assert_eq!(params.query, "Hello, Claude!");
    assert!(params.working_dir.is_none());
    assert_eq!(params.tags, vec!["ci"]);
}

#[test]
fn test_launch_session_params_with_working_dir() {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "LaunchSession",
        "params": {
            "query": "Hello",
            "working_dir": "/tmp/test",
            "tags": ["ci"]
        }
    });

    let parsed: RpcRequest = serde_json::from_value(request).expect("Valid request");

    let params: rsi_common::rpc::LaunchSessionParams =
        serde_json::from_value(parsed.params).expect("Valid params");

    assert_eq!(params.query, "Hello");
    assert_eq!(params.working_dir, Some(PathBuf::from("/tmp/test")));
    assert_eq!(params.tags, vec!["ci"]);
}

#[test]
fn test_list_sessions_empty_params() {
    // ListSessions doesn't require params, verify it works with null
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "ListSessions"
    });

    let parsed: RpcRequest = serde_json::from_value(request).expect("Valid request");
    assert_eq!(parsed.method, "ListSessions");
    // params should default to null
    assert!(parsed.params.is_null());
}

#[test]
fn test_response_success_format() {
    let response = RpcResponse::success(
        Some(serde_json::json!(1)),
        serde_json::json!({"session_id": "abc123"}),
    );

    assert_eq!(response.jsonrpc, "2.0");
    assert!(response.result.is_some());
    assert!(response.error.is_none());
}

#[test]
fn test_response_error_format() {
    use rsi_common::rpc::{METHOD_NOT_FOUND, RpcError};

    let response = RpcResponse::error(
        Some(serde_json::json!(1)),
        RpcError {
            code: METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
            data: None,
        },
    );

    assert_eq!(response.jsonrpc, "2.0");
    assert!(response.result.is_none());
    assert!(response.error.is_some());
    assert_eq!(response.error.unwrap().code, METHOD_NOT_FOUND);
}

#[test]
fn test_health_status_response_format() {
    let response = rsi_common::rpc::HealthStatusResponse {
        persistence_queue_depth: 5,
        persistence_queue_capacity: 256,
        last_command_duration_ms: 42,
        project_cache_size: 3,
        project_cache_hits: 0,
        project_cache_misses: 0,
        last_poll_payload_bytes: 0,
        last_poll_event_count: 0,
        provider_claude_available: true,
        provider_codex_available: false,
        provider_pioneer_available: false,
        provider_openrouter_available: false,
        provider_bedrock_available: false,
        provider_local_available: false,
        provider_antigravity_available: false,
        provider_codex_app_server_available: false,
        provider_harness_available: true,
        queue_pending: 10,
        queue_claimed: 2,
        queue_completed: 50,
        queue_failed: 1,
        rate_limits: Vec::new(),
        latest_daemon_restart: None,
    };

    let json = serde_json::to_value(&response).expect("serialize HealthStatusResponse");
    assert_eq!(json["persistence_queue_depth"], 5);
    assert_eq!(json["persistence_queue_capacity"], 256);
    assert_eq!(json["last_command_duration_ms"], 42);
    assert_eq!(json["project_cache_size"], 3);
    assert_eq!(json["queue_pending"], 10);
    assert_eq!(json["queue_claimed"], 2);
    assert_eq!(json["queue_completed"], 50);
    assert_eq!(json["queue_failed"], 1);

    // Round-trip
    let parsed: rsi_common::rpc::HealthStatusResponse =
        serde_json::from_value(json).expect("deserialize HealthStatusResponse");
    assert_eq!(parsed.persistence_queue_depth, 5);
    assert_eq!(parsed.last_command_duration_ms, 42);
}

// Integration test that requires the daemon to be running
// Run with: cargo test -p flywheeld --test integration -- --ignored
#[tokio::test]
#[ignore = "Requires daemon to be running"]
async fn test_daemon_list_sessions() {
    let socket_path = dirs::home_dir()
        .map(|p| p.join(".flywheel/daemon.sock"))
        .expect("No home directory");

    // Connect to daemon
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("Failed to connect to daemon - is it running?");

    // Send ListSessions request
    let request = RpcRequest::new("ListSessions", serde_json::Value::Null);
    let response = send_rpc(&mut stream, &request).await;

    // Verify response
    assert!(
        response.error.is_none(),
        "Unexpected error: {:?}",
        response.error
    );
    assert!(response.result.is_some());

    // Result should be an array (possibly empty)
    let sessions = response.result.unwrap();
    assert!(sessions.is_array(), "Expected array, got: {:?}", sessions);
}
