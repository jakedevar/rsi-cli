//! Token-bound Unix JSON-RPC transport shared by agent control clients.
//!
//! The session token is read only from the inherited environment.  It is not
//! part of a tool schema or caller-provided request payload.

use crate::identity;
#[cfg(unix)]
use crate::rpc::RpcRequest;
use crate::rpc::RpcResponse;
use serde_json::Value;
#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

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
    dispatch_with_timeout(socket, method, params, default_timeout(method))
}

// Settlement may prove and remove hundreds of roots. The daemon owns the run
// through disconnects; this budget lets synchronous clients receive its receipt.
// A landing enqueue with `derive_filters` runs the touched-shard derivation and
// the source probe synchronously (a large diff took longer than 30 s, #2357),
// so it gets a budget that covers them rather than a false transport failure.
fn default_timeout(method: &str) -> Duration {
    Duration::from_secs(match method {
        "AgentApplySourceWorktreeCohort" => 1800,
        "AgentEnqueueLandingSource" | "EnqueueRollingQueueSource" => 300,
        _ => 30,
    })
}

/// The request was fully written, but no complete response arrived within the
/// timeout (#2357). Unlike a connect or write timeout, the daemon may already
/// have processed the call, so the caller must replay it under the same
/// `idempotency_key` rather than treat it as not-done or retry under a new key.
/// Carried inside the returned [`std::io::Error`] (kind `TimedOut`); recover it
/// with [`response_timeout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponseTimeout {
    pub method: String,
    pub timeout: Duration,
}

impl RpcResponseTimeout {
    /// Stable machine-readable code for the typed error.
    pub const CODE: &'static str = "rpc_response_timeout";
}

/// An advertised read: an `Agent` Get/List/Audit/Query/Read verb whose schema
/// has no `idempotency_key` (#2622). It changes nothing, so a response timeout
/// is answered by asking again, not by replaying a key it does not have.
fn is_keyless_agent_read(method: &str) -> bool {
    const READ_PREFIXES: [&str; 5] = [
        "AgentGet",
        "AgentList",
        "AgentAudit",
        "AgentQuery",
        "AgentRead",
    ];
    READ_PREFIXES
        .iter()
        .any(|prefix| method.starts_with(prefix))
        && crate::agent_control_schema::agent_control_catalog_v1()
            .iter()
            .find(|descriptor| descriptor.method == method)
            .is_some_and(|descriptor| !descriptor.parameters_json().contains("\"idempotency_key\""))
}

impl std::fmt::Display for RpcResponseTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if is_keyless_agent_read(&self.method) {
            return write!(
                f,
                "{}: {} was sent but no response arrived within {}s. It is a read-only call \
                 with no idempotency_key and changed nothing; ask again. A daemon that is \
                 busy answers with a typed busy refusal rather than silence",
                Self::CODE,
                self.method,
                self.timeout.as_secs()
            );
        }
        write!(
            f,
            "{}: {} was sent but no response arrived within {}s; the call may have \
             completed. Replay it with the same idempotency_key and identical params \
             (an exact replay returns the original receipt), or read the state, before \
             issuing a new decision",
            Self::CODE,
            self.method,
            self.timeout.as_secs()
        )
    }
}

impl std::error::Error for RpcResponseTimeout {}

/// The typed response timeout inside a transport error, if that is what it is.
pub fn response_timeout(error: &std::io::Error) -> Option<&RpcResponseTimeout> {
    error.get_ref()?.downcast_ref::<RpcResponseTimeout>()
}

#[cfg(unix)]
fn response_timeout_error(method: &str, timeout: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        RpcResponseTimeout {
            method: method.to_string(),
            timeout,
        },
    )
}

/// Like [`dispatch`], with one timeout budget for connect, write and read
/// (#1049: the tool-boundary hook must never wait long on the daemon).
/// Missing/refused sockets are retried during a deploy gap. Once connected,
/// requests are never replayed: a write/read failure may follow a committed RPC.
/// Platforms without Unix sockets return [`std::io::ErrorKind::Unsupported`].
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
    #[cfg(unix)]
    {
        dispatch_unix(socket, method, params, timeout)
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, method, params);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "daemon RPC transport requires Unix sockets on this platform",
        ))
    }
}

#[cfg(unix)]
fn dispatch_unix(
    socket: &Path,
    method: &str,
    params: Value,
    timeout: Duration,
) -> std::io::Result<RpcResponse> {
    let started = Instant::now();
    let mut stream = connect_with_retry(socket, started, timeout)?;
    let request = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(Value::Number(1.into())),
        method: method.to_string(),
        params,
        session_token: std::env::var(identity::ENV_SESSION_TOKEN)
            .ok()
            .filter(|value| !value.is_empty())
            .map(Into::into),
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
        let left =
            remaining(started, timeout).map_err(|_| response_timeout_error(method, timeout))?;
        stream.set_read_timeout(Some(left))?;
        let read = match stream.read(&mut buf) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            // SO_RCVTIMEO expiry reads as EAGAIN ("Resource temporarily
            // unavailable", os error 11), which looks like a busy socket.
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(response_timeout_error(method, timeout));
            }
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

#[cfg(unix)]
fn remaining(started: Instant, timeout: Duration) -> std::io::Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "daemon RPC timeout expired")
        })
}

#[cfg(unix)]
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

#[cfg(all(test, unix))]
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
    fn slow_response_is_a_typed_replayable_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(read_request(&stream)["method"], "AgentEnqueueLandingSource");
            thread::sleep(Duration::from_millis(600));
            drop(stream);
        });
        let err = dispatch_with_timeout(
            &socket,
            "AgentEnqueueLandingSource",
            json!({}),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let typed = response_timeout(&err).expect("typed response timeout");
        assert_eq!(typed.method, "AgentEnqueueLandingSource");
        let text = err.to_string();
        assert!(text.starts_with("rpc_response_timeout: AgentEnqueueLandingSource"));
        assert!(text.contains("may have completed"));
        assert!(text.contains("same idempotency_key"));
        server.join().unwrap();
    }

    #[test]
    fn keyless_read_timeout_does_not_advise_an_idempotency_replay() {
        for method in ["AgentAuditSourceWorktreeCohort", "AgentGetStatus"] {
            let text = RpcResponseTimeout {
                method: method.into(),
                timeout: Duration::from_secs(30),
            }
            .to_string();
            assert!(text.starts_with(&format!("rpc_response_timeout: {method}")));
            assert!(!text.contains("idempotency_key and identical"), "{text}");
            assert!(!text.contains("Replay it"), "{text}");
            assert!(text.contains("read-only"), "{text}");
        }
        // A keyed mutation keeps the replay advice.
        let apply = RpcResponseTimeout {
            method: "AgentApplySourceWorktreeCohort".into(),
            timeout: Duration::from_secs(1800),
        }
        .to_string();
        assert!(apply.contains("same idempotency_key"), "{apply}");
    }

    #[test]
    fn connect_timeout_is_not_a_response_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let err = dispatch_with_timeout(
            &dir.path().join("missing.sock"),
            "AgentGetStatus",
            json!({}),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(response_timeout(&err).is_none());
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

#[cfg(test)]
mod portable_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn landing_enqueue_gets_a_derivation_sized_timeout() {
        for method in ["AgentEnqueueLandingSource", "EnqueueRollingQueueSource"] {
            assert_eq!(default_timeout(method), Duration::from_secs(300));
        }
    }

    #[test]
    fn settlement_apply_gets_a_cohort_sized_timeout_only() {
        assert_eq!(
            default_timeout("AgentApplySourceWorktreeCohort"),
            Duration::from_secs(1800)
        );
        assert_eq!(
            default_timeout("AgentAuditSourceWorktreeCohort"),
            Duration::from_secs(30)
        );
        assert_eq!(default_timeout("AgentGetStatus"), Duration::from_secs(30));
    }

    #[test]
    fn zero_timeout_is_invalid_on_every_platform() {
        let err = dispatch_with_timeout(
            Path::new("daemon.sock"),
            "AgentGetStatus",
            json!({}),
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(not(unix))]
    #[test]
    fn rpc_entry_points_report_unsupported_transport() {
        let socket = Path::new("daemon.sock");
        for result in [
            dispatch_from_env("AgentGetStatus", json!({})),
            dispatch(socket, "AgentGetStatus", json!({})),
            dispatch_with_timeout(socket, "AgentGetStatus", json!({}), Duration::from_secs(1)),
        ] {
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
        }
    }
}
