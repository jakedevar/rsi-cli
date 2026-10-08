//! Token-bound Unix JSON-RPC transport shared by agent control clients.
//!
//! The session token is read only from the inherited environment.  It is not
//! part of a tool schema or caller-provided request payload.

use crate::identity;
use crate::rpc::{RpcRequest, RpcResponse};
use serde_json::Value;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn resolve_socket_path() -> PathBuf {
    std::env::var("RSI_DAEMON_SOCKET_PATH")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(identity::default_socket_path)
}

pub fn dispatch_from_env(method: &str, params: Value) -> std::io::Result<RpcResponse> {
    dispatch(&resolve_socket_path(), method, params)
}

pub fn dispatch(socket: &Path, method: &str, params: Value) -> std::io::Result<RpcResponse> {
    dispatch_with_timeout(socket, method, params, Duration::from_secs(30))
}

/// Like [`dispatch`], with one timeout budget for connect, write and read
/// (#1049: the tool-boundary hook must never wait long on the daemon).
/// Missing/refused sockets are retried during a deploy gap. Once connected,
/// requests are never replayed: a write/read failure may follow a committed RPC.
pub fn dispatch_with_timeout(
    socket: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
) -> std::io::Result<RpcResponse> {
    if timeout.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon RPC timeout must be nonzero",
        ));
    }
    let started = Instant::now();
    let mut stream = connect_with_retry(socket, started, timeout)?;
    let request = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(Value::Number(1.into())),
        method: method.to_string(),
        params,
        session_token: std::env::var(identity::ENV_SESSION_TOKEN)
            .ok()
            .filter(|value| !value.is_empty()),
    };
    let mut request_json = serde_json::to_vec(&request)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    request_json.push(b'\n');
    let mut pending = request_json.as_slice();
    while !pending.is_empty() {
        stream.set_write_timeout(Some(remaining(started, timeout)?))?;
        match stream.write(pending) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => pending = &pending[written..],
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    let mut response = Vec::new();
    let mut buf = [0; 4096];
    loop {
        stream.set_read_timeout(Some(remaining(started, timeout)?))?;
        let read = match stream.read(&mut buf) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if read == 0 {
            break;
        }
        if let Some(newline) = buf[..read].iter().position(|byte| *byte == b'\n') {
            response.extend_from_slice(&buf[..=newline]);
            break;
        }
        response.extend_from_slice(&buf[..read]);
    }
    if response.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed connection without a response",
        ));
    }
    serde_json::from_slice(&response)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

fn remaining(started: Instant, timeout: Duration) -> std::io::Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "daemon RPC timeout expired")
        })
}

fn connect_with_retry(
    socket: &Path,
    started: Instant,
    timeout: Duration,
) -> std::io::Result<UnixStream> {
    let mut backoff = Duration::from_millis(25);
    loop {
        remaining(started, timeout)?;
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                std::thread::sleep(backoff.min(remaining(started, timeout)?));
                backoff = (backoff * 2).min(Duration::from_millis(250));
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead as _;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn read_request(stream: &UnixStream) -> Value {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut request)
            .unwrap();
        serde_json::from_str(&request).unwrap()
    }

    #[test]
    fn dispatch_retries_when_socket_appears_after_300ms() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let listener = UnixListener::bind(server_socket).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&stream);
            writeln!(
                stream,
                "{}",
                json!({"jsonrpc":"2.0", "id":1, "result":{"ready":true}})
            )
            .unwrap();
            request
        });
        let response = dispatch(&socket, "AgentGetStatus", json!({"probe":true})).unwrap();
        assert_eq!(response.result, Some(json!({"ready":true})));
        let request = server.join().unwrap();
        assert_eq!(request["method"], "AgentGetStatus");
        assert_eq!(request["params"], json!({"probe":true}));
    }

    #[test]
    fn dispatch_retries_refused_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert_eq!(
            UnixStream::connect(&socket).unwrap_err().kind(),
            std::io::ErrorKind::ConnectionRefused
        );
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            std::fs::remove_file(&server_socket).unwrap();
            let listener = UnixListener::bind(server_socket).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&stream);
            writeln!(
                stream,
                "{}",
                json!({"jsonrpc":"2.0", "id":1, "result":"ready"})
            )
            .unwrap();
        });
        let response =
            dispatch_with_timeout(&socket, "AgentGetStatus", json!({}), Duration::from_secs(2))
                .unwrap();
        assert_eq!(response.result, Some(json!("ready")));
        server.join().unwrap();
    }

    #[test]
    fn missing_socket_expires_within_hook_budget() {
        let dir = tempfile::tempdir().unwrap();
        let budget = crate::boundary_mail_hook::CONFIRM_TIMEOUT;
        let started = Instant::now();
        let err = dispatch_with_timeout(
            &dir.path().join("missing.sock"),
            "ConfirmBoundaryMail",
            json!({}),
            budget,
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= budget);
        assert!(started.elapsed() < budget + Duration::from_millis(500));
    }

    #[test]
    fn invalid_socket_path_fails_without_retry() {
        let started = Instant::now();
        let err = dispatch_with_timeout(
            Path::new("invalid\0socket"),
            "AgentGetStatus",
            json!({}),
            Duration::from_secs(30),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn response_timeout_uses_budget_left_after_connect_retry() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let listener = UnixListener::bind(server_socket).unwrap();
            let (stream, _) = listener.accept().unwrap();
            read_request(&stream);
            // Keep the socket open past the client's whole-call budget.
            thread::sleep(Duration::from_millis(400));
        });
        let started = Instant::now();
        let err = dispatch_with_timeout(
            &socket,
            "AgentGetStatus",
            json!({}),
            Duration::from_millis(600),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(elapsed < Duration::from_millis(850), "elapsed: {elapsed:?}");
        server.join().unwrap();
    }

    #[test]
    fn trickling_response_cannot_extend_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&stream);
            for _ in 0..30 {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        });
        let started = Instant::now();
        let err = dispatch_with_timeout(
            &socket,
            "AgentGetStatus",
            json!({}),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_millis(600));
        server.join().unwrap();
    }

    #[test]
    fn request_is_not_replayed_after_server_closes_connection() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(read_request(&stream)["method"], "AgentSendMessage");
            drop(stream);
            listener
        });
        let err = dispatch_with_timeout(
            &socket,
            "AgentSendMessage",
            json!({}),
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
