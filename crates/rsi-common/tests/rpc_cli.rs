use std::io::{BufRead as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

use rsi_common::rpc::{METHOD_NOT_FOUND, RpcError, RpcRequest, RpcResponse};

const BIN: &str = env!("CARGO_BIN_EXE_rsi-rpc");

fn spawn_mock_daemon(
    response: RpcResponse,
) -> Option<(tempfile::TempDir, PathBuf, thread::JoinHandle<()>)> {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("create tempdir: {err}"),
    };
    let socket = dir.path().join("daemon.sock");
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping rsi-rpc socket test: Unix socket bind denied by host sandbox");
            return None;
        }
        Err(err) => panic!("bind mock daemon socket: {err}"),
    };
    let handle = thread::spawn(move || {
        let (mut stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(err) => panic!("accept rsi-rpc connection: {err}"),
        };
        let mut line = String::new();
        {
            let mut reader = std::io::BufReader::new(&mut stream);
            if let Err(err) = reader.read_line(&mut line) {
                panic!("read request line: {err}");
            }
        }
        let request: RpcRequest = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(err) => panic!("request should be valid JSON-RPC: {err}; line={line}"),
        };
        assert_eq!(request.method, "ListSessions");
        let response_json = match serde_json::to_string(&response) {
            Ok(json) => json,
            Err(err) => panic!("serialize mock response: {err}"),
        };
        if let Err(err) = writeln!(stream, "{response_json}") {
            panic!("write mock response: {err}");
        }
    });
    Some((dir, socket, handle))
}

#[test]
fn list_sessions_returns_json_response() {
    let response = RpcResponse::success(Some(serde_json::json!(1)), serde_json::json!([]));
    let Some((_dir, socket, handle)) = spawn_mock_daemon(response) else {
        return;
    };

    let output = match Command::new(BIN)
        .env("RSI_DAEMON_SOCKET_PATH", &socket)
        .arg("--")
        .arg("ListSessions")
        .output()
    {
        Ok(output) => output,
        Err(err) => panic!("spawn rsi-rpc: {err}"),
    };
    if let Err(err) = handle.join() {
        panic!("mock daemon thread panicked: {err:?}");
    }

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(parsed) => parsed,
        Err(err) => panic!("stdout should be JSON: {err}"),
    };
    assert!(parsed.get("result").is_some());
}

#[test]
fn rpc_errors_exit_nonzero() {
    let response = RpcResponse::error(
        Some(serde_json::json!(1)),
        RpcError {
            code: METHOD_NOT_FOUND,
            message: "Method not found: ListSessions".to_string(),
            data: None,
        },
    );
    let Some((_dir, socket, handle)) = spawn_mock_daemon(response) else {
        return;
    };

    let output = match Command::new(BIN)
        .env("RSI_DAEMON_SOCKET_PATH", &socket)
        .arg("ListSessions")
        .output()
    {
        Ok(output) => output,
        Err(err) => panic!("spawn rsi-rpc: {err}"),
    };
    if let Err(err) = handle.join() {
        panic!("mock daemon thread panicked: {err:?}");
    }

    assert_eq!(output.status.code(), Some(2));
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(parsed) => parsed,
        Err(err) => panic!("stdout should be JSON: {err}"),
    };
    assert!(parsed.get("error").is_some());
}

#[test]
fn pipeline_verify_refuses_user_default_socket() {
    let home = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("create temp home: {err}"),
    };
    let output = match Command::new(BIN)
        .env("HOME", home.path())
        .env("CLAUDE_AGENT_ROLE", "pipeline-verify")
        .env_remove("RSI_DAEMON_SOCKET_PATH")
        .env_remove("RSI_SOCKET")
        .env_remove("MOTHERSHIP_SOCKET")
        .env_remove("FLYWHEEL_SOCKET")
        .arg("ListSessions")
        .output()
    {
        Ok(output) => output,
        Err(err) => panic!("spawn rsi-rpc: {err}"),
    };

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to dispatch pipeline-verify RPCs"));
}
