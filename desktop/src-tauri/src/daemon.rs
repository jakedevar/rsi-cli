//! Minimal JSON-RPC 2.0 client for the rsid Unix socket.
//!
//! Framing matches `crates/rsi/src/client.rs`: one request object per line,
//! one response object per line. Each call opens its own connection so a
//! slow read never blocks another command, and every call is bounded in time
//! and response size. No session token is ever sent: the desktop app is an
//! operator surface, authenticated by owning the socket.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Upper bound on one response line (large conversations included).
pub const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound on one round trip.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// The socket could not be reached (daemon down or wrong path).
    Connect(String),
    /// I/O failed mid-exchange or the daemon closed the connection.
    Io(String),
    /// The daemon did not answer in time.
    Timeout,
    /// The response line exceeded [`MAX_RESPONSE_BYTES`].
    TooLarge,
    /// The response was not a JSON-RPC response object.
    Malformed(String),
    /// The daemon answered with a JSON-RPC error object.
    Rpc { code: i64, message: String },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "cannot reach rsid: {e}"),
            Self::Io(e) => write!(f, "daemon i/o error: {e}"),
            Self::Timeout => write!(f, "daemon did not answer in time"),
            Self::TooLarge => write!(f, "daemon response too large"),
            Self::Malformed(e) => write!(f, "malformed daemon response: {e}"),
            Self::Rpc { code, message } => write!(f, "daemon error {code}: {message}"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Resolve the daemon socket: `$RSI_DAEMON_SOCKET`, else `~/.rsi/daemon.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("RSI_DAEMON_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".rsi").join("daemon.sock")
}

#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
    deadline: Duration,
}

impl DaemonClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            deadline: DEFAULT_DEADLINE,
        }
    }

    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Send one request and return its `result` (`null` when absent).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, DaemonError> {
        tokio::time::timeout(self.deadline, self.exchange(method, params))
            .await
            .map_err(|_| DaemonError::Timeout)?
    }

    async fn exchange(&self, method: &str, params: Value) -> Result<Value, DaemonError> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| DaemonError::Connect(format!("{}: {e}", self.socket.display())))?;
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&request).map_err(|e| DaemonError::Io(e.to_string()))?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .await
            .map_err(|e| DaemonError::Io(e.to_string()))?;
        stream
            .flush()
            .await
            .map_err(|e| DaemonError::Io(e.to_string()))?;

        let mut reader = BufReader::new(stream.take(MAX_RESPONSE_BYTES + 1));
        let mut buf = Vec::new();
        let n = reader
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| DaemonError::Io(e.to_string()))?;
        if n == 0 {
            return Err(DaemonError::Io(
                "connection closed before a response".into(),
            ));
        }
        if buf.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(DaemonError::TooLarge);
        }
        parse_response(&buf)
    }
}

/// Map one JSON-RPC response line to its result or error.
pub fn parse_response(line: &[u8]) -> Result<Value, DaemonError> {
    let value: Value =
        serde_json::from_slice(line).map_err(|e| DaemonError::Malformed(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| DaemonError::Malformed("response is not an object".into()))?;
    if let Some(err) = obj.get("error").filter(|e| !e.is_null()) {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        return Err(DaemonError::Rpc { code, message });
    }
    Ok(obj.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;

    fn fake_daemon(dir: &Path, reply: &'static str) -> (PathBuf, Arc<Mutex<Option<String>>>) {
        let sock = dir.join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(None));
        let seen2 = Arc::clone(&seen);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            *seen2.lock().unwrap() = Some(line);
            w.write_all(reply.as_bytes()).await.unwrap();
        });
        (sock, seen)
    }

    #[tokio::test]
    async fn sends_one_line_request_without_token_and_returns_result() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, seen) = fake_daemon(
            dir.path(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":[1,2]}\n",
        );
        let out = DaemonClient::new(&sock)
            .call("ListSessions", Value::Null)
            .await
            .unwrap();
        assert_eq!(out, json!([1, 2]));
        let sent: Value = serde_json::from_str(seen.lock().unwrap().as_deref().unwrap()).unwrap();
        assert_eq!(sent["method"], "ListSessions");
        assert_eq!(sent["jsonrpc"], "2.0");
        assert!(sent.get("session_token").is_none());
    }

    #[tokio::test]
    async fn rpc_error_object_maps_to_rpc_error() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, _) = fake_daemon(
            dir.path(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"nope\"}}\n",
        );
        let err = DaemonClient::new(&sock)
            .call("Bogus", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            DaemonError::Rpc {
                code: -32601,
                message: "nope".into()
            }
        );
    }

    #[tokio::test]
    async fn missing_socket_is_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = DaemonClient::new(dir.path().join("absent.sock"))
            .call("ListSessions", Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, DaemonError::Connect(_)));
    }

    #[tokio::test]
    async fn silent_daemon_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let err = DaemonClient::new(&sock)
            .with_deadline(Duration::from_millis(100))
            .call("ListSessions", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(err, DaemonError::Timeout);
    }

    #[test]
    fn non_object_and_garbage_are_malformed() {
        assert!(matches!(
            parse_response(b"[1]"),
            Err(DaemonError::Malformed(_))
        ));
        assert!(matches!(
            parse_response(b"nope"),
            Err(DaemonError::Malformed(_))
        ));
    }

    #[test]
    fn absent_result_is_null() {
        assert_eq!(
            parse_response(b"{\"jsonrpc\":\"2.0\",\"id\":1}").unwrap(),
            Value::Null
        );
    }
}
