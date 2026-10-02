//! Closed, bounded adapter from the six gateway routes to the six operator-only
//! daemon reads.
//!
//! Each route accepts only the query keys it defines: cursors map to their own
//! cursor kind, and `before` anchors an older history window.
//!
//! Denied requests are refused in `build_request` before any dispatch, so a
//! denial provably causes zero daemon traffic.

use crate::{config::Config, ingress};
use rsi_common::remote_read::ReadRequestV1;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Semaphore,
    time::timeout,
};

/// Cap on a single daemon response line.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadDenial {
    NoRoute,
    ProjectOutOfScope,
    InvalidRequest,
}

impl ReadDenial {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoRoute => "no_route",
            Self::ProjectOutOfScope => "project_out_of_scope",
            Self::InvalidRequest => "invalid_request",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    Busy,
    Timeout,
    Connect,
    PeerMismatch,
    Oversize,
    Malformed,
    Daemon(i64),
}

impl ReadError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::PeerMismatch => "peer_mismatch",
            Self::Oversize => "oversize",
            Self::Malformed => "malformed",
            Self::Daemon(_) => "daemon_error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeError {
    Denied(ReadDenial),
    Failed(ReadError),
}

/// A closed route table is the only way a path becomes a request. All project
/// policy checks happen here, before anything is dispatched.
///
/// # Errors
///
/// Returns [`ReadDenial::NoRoute`] for an unrecognized path,
/// [`ReadDenial::ProjectOutOfScope`] for a project outside `policy.project_ids`,
/// and [`ReadDenial::InvalidRequest`] when the wire types reject the request.
pub fn build_request(
    path: &str,
    query: &ingress::Query,
    policy: &Config,
) -> Result<ReadRequestV1, ReadDenial> {
    let method = ingress::read_method(path).ok_or(ReadDenial::NoRoute)?;
    let segments: Vec<&str> = path.split('/').collect();
    let project = segments.get(4).copied();
    let session = segments.get(6).copied();
    if let Some(project) = project
        && !policy.project_ids.iter().any(|allowed| allowed == project)
    {
        return Err(ReadDenial::ProjectOutOfScope);
    }
    let cursor_kind = match method {
        "RemoteListProjectsV1" => Some("projects"),
        "RemoteListSessionsV1" => Some("sessions"),
        "RemoteGetDecisionsV1" => Some("decisions"),
        "RemoteGetHistoryPageV1" => Some("history"),
        _ => None,
    };
    if query.cursor.is_some() && cursor_kind.is_none() {
        return Err(ReadDenial::InvalidRequest);
    }
    if query.before.is_some() && method != "RemoteGetHistoryPageV1" {
        return Err(ReadDenial::InvalidRequest);
    }
    let cursor = query
        .cursor
        .as_ref()
        .map(|token| json!({ "kind": cursor_kind, "token": token }));
    let request = match method {
        "RemoteGetInfoV1" => json!({ "method": method, "params": {} }),
        "RemoteListProjectsV1" => json!({
            "method": method,
            "params": { "project_ids": policy.project_ids, "limit": 25, "cursor": cursor },
        }),
        "RemoteListSessionsV1" => json!({
            "method": method,
            "params": { "project_id": project, "limit": 50, "cursor": cursor },
        }),
        "RemoteGetSessionV1" => json!({
            "method": method,
            "params": { "project_id": project, "session_id": session },
        }),
        "RemoteGetHistoryPageV1" => json!({
            "method": method,
            "params": {
                "project_id": project,
                "session_id": session,
                "window": match query.before {
                    Some((sequence, id)) => json!({
                        "kind": "older",
                        "anchor": { "sequence": sequence, "id": id.to_string() },
                    }),
                    None => json!({ "kind": "latest" }),
                },
                "limit": 25,
                "cursor": cursor,
            },
        }),
        "RemoteGetDecisionsV1" => json!({
            "method": method,
            "params": {
                "project_id": project,
                "session_id": session,
                "limit": 16,
                "mode": "attention",
                "cursor": cursor,
                "selected_decision_id": null,
            },
        }),
        _ => return Err(ReadDenial::NoRoute),
    };
    serde_json::from_value(request).map_err(|_| ReadDenial::InvalidRequest)
}

/// Transport for one already-authorized request. Implementations must not be
/// reachable from a denial path.
pub trait ReadDispatch: Send + Sync {
    fn dispatch(
        &self,
        request: &ReadRequestV1,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ReadError>> + Send;
}

pub struct DaemonReads {
    socket: PathBuf,
    deadline: Duration,
    permits: Arc<Semaphore>,
}

impl DaemonReads {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            deadline: Duration::from_secs(5),
            permits: Arc::new(Semaphore::new(4)),
        }
    }

    #[must_use]
    pub const fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }

    async fn exchange(&self, request: &ReadRequestV1) -> Result<Vec<u8>, ReadError> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| ReadError::Connect)?;
        let peer = stream.peer_cred().map_err(|_| ReadError::Connect)?;
        // SAFETY: geteuid has no preconditions and dereferences no pointers.
        if peer.uid() != unsafe { libc::geteuid() } {
            return Err(ReadError::PeerMismatch);
        }
        let mut value = serde_json::to_value(request).map_err(|_| ReadError::Malformed)?;
        let object = value.as_object_mut().ok_or(ReadError::Malformed)?;
        // No session token is ever attached: the daemon read RPCs are
        // operator-only and this transport must not imply agent authority.
        object.insert("jsonrpc".to_string(), json!("2.0"));
        object.insert("id".to_string(), json!(1));
        let mut line = serde_json::to_vec(&value).map_err(|_| ReadError::Malformed)?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .await
            .map_err(|_| ReadError::Connect)?;
        let mut response = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|_| ReadError::Connect)?;
            if n == 0 {
                break;
            }
            if let Some(pos) = chunk[..n].iter().position(|b| *b == b'\n') {
                response.extend_from_slice(&chunk[..pos]);
                break;
            }
            response.extend_from_slice(&chunk[..n]);
            if response.len() > MAX_RESPONSE_BYTES {
                return Err(ReadError::Oversize);
            }
        }
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(ReadError::Oversize);
        }
        let parsed: Value = serde_json::from_slice(&response).map_err(|_| ReadError::Malformed)?;
        let object = parsed.as_object().ok_or(ReadError::Malformed)?;
        if let Some(error) = object.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
            return Err(ReadError::Daemon(code));
        }
        let result = object.get("result").ok_or(ReadError::Malformed)?;
        if !result.is_object() {
            return Err(ReadError::Malformed);
        }
        serde_json::to_vec(result).map_err(|_| ReadError::Malformed)
    }
}

impl ReadDispatch for DaemonReads {
    async fn dispatch(&self, request: &ReadRequestV1) -> Result<Vec<u8>, ReadError> {
        let _permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ReadError::Busy)?;
        timeout(self.deadline, self.exchange(request))
            .await
            .map_err(|_| ReadError::Timeout)?
    }
}

/// `RSI_SOCKET` when set to an absolute path, else `$HOME/.rsi/daemon.sock`.
#[must_use]
pub fn default_daemon_socket() -> Option<PathBuf> {
    if let Some(socket) = std::env::var_os("RSI_SOCKET") {
        let socket = PathBuf::from(socket);
        if socket.is_absolute() {
            return Some(socket);
        }
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".rsi/daemon.sock"))
}

/// # Errors
///
/// Returns [`ServeError::Denied`] before any dispatch when the path is not
/// authorized, or [`ServeError::Failed`] with the dispatch transport error.
pub async fn serve_read<D: ReadDispatch>(
    path: &str,
    query: &ingress::Query,
    policy: &Config,
    dispatch: &D,
) -> Result<Vec<u8>, ServeError> {
    let request = build_request(path, query, policy).map_err(ServeError::Denied)?;
    dispatch
        .dispatch(&request)
        .await
        .map_err(ServeError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::Query;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::net::UnixListener;

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const SESSION: &str = "550e8400-e29b-41d4-a716-446655440001";
    const OTHER: &str = "550e8400-e29b-41d4-a716-4466554400ff";

    fn policy() -> Config {
        Config {
            enabled: true,
            canonical_host: "host.example.ts.net".to_string(),
            owner_user_id: 501,
            allowed_node_ids: vec!["nHOME".to_string()],
            project_ids: vec![PROJECT.to_string()],
        }
    }

    fn project_path(suffix: &str) -> String {
        format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}{suffix}")
    }

    fn query(cursor: Option<&str>, before: Option<(i32, i64)>) -> Query {
        Query {
            cursor: cursor.map(str::to_string),
            before,
        }
    }

    struct CountingDispatch {
        calls: AtomicUsize,
    }

    impl CountingDispatch {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ReadDispatch for CountingDispatch {
        async fn dispatch(&self, _request: &ReadRequestV1) -> Result<Vec<u8>, ReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(b"{}".to_vec())
        }
    }

    #[test]
    fn request_shapes_use_the_closed_route_table() {
        let policy = policy();
        let cases: Vec<(String, Value)> = vec![
            (
                "/api/v1/info".to_string(),
                json!({ "method": "RemoteGetInfoV1", "params": {} }),
            ),
            (
                "/api/v1/projects".to_string(),
                json!({
                    "method": "RemoteListProjectsV1",
                    "params": { "project_ids": [PROJECT], "limit": 25, "cursor": null },
                }),
            ),
            (
                format!("/api/v1/projects/{PROJECT}/sessions"),
                json!({
                    "method": "RemoteListSessionsV1",
                    "params": { "project_id": PROJECT, "limit": 50, "cursor": null },
                }),
            ),
            (
                project_path(""),
                json!({
                    "method": "RemoteGetSessionV1",
                    "params": { "project_id": PROJECT, "session_id": SESSION },
                }),
            ),
            (
                project_path("/history"),
                json!({
                    "method": "RemoteGetHistoryPageV1",
                    "params": {
                        "project_id": PROJECT,
                        "session_id": SESSION,
                        "window": { "kind": "latest" },
                        "limit": 25,
                        "cursor": null,
                    },
                }),
            ),
            (
                project_path("/decisions"),
                json!({
                    "method": "RemoteGetDecisionsV1",
                    "params": {
                        "project_id": PROJECT,
                        "session_id": SESSION,
                        "limit": 16,
                        "mode": "attention",
                        "cursor": null,
                        "selected_decision_id": null,
                    },
                }),
            ),
        ];
        for (path, expected) in cases {
            let request =
                build_request(&path, &Query::default(), &policy).expect("in-scope route builds");
            assert_eq!(serde_json::to_value(&request).unwrap(), expected);
        }
    }

    #[test]
    fn query_maps_to_cursors_and_older_windows() {
        let policy = policy();
        let cases: Vec<(String, Query, Value)> = vec![
            (
                "/api/v1/projects".to_string(),
                query(Some("tok"), None),
                json!({
                    "method": "RemoteListProjectsV1",
                    "params": {
                        "project_ids": [PROJECT],
                        "limit": 25,
                        "cursor": { "kind": "projects", "token": "tok" },
                    },
                }),
            ),
            (
                format!("/api/v1/projects/{PROJECT}/sessions"),
                query(Some("abc_-9"), None),
                json!({
                    "method": "RemoteListSessionsV1",
                    "params": {
                        "project_id": PROJECT,
                        "limit": 50,
                        "cursor": { "kind": "sessions", "token": "abc_-9" },
                    },
                }),
            ),
            (
                project_path("/decisions"),
                query(Some("d1"), None),
                json!({
                    "method": "RemoteGetDecisionsV1",
                    "params": {
                        "project_id": PROJECT,
                        "session_id": SESSION,
                        "limit": 16,
                        "mode": "attention",
                        "cursor": { "kind": "decisions", "token": "d1" },
                        "selected_decision_id": null,
                    },
                }),
            ),
            (
                project_path("/history"),
                query(Some("h1"), None),
                json!({
                    "method": "RemoteGetHistoryPageV1",
                    "params": {
                        "project_id": PROJECT,
                        "session_id": SESSION,
                        "window": { "kind": "latest" },
                        "limit": 25,
                        "cursor": { "kind": "history", "token": "h1" },
                    },
                }),
            ),
            (
                project_path("/history"),
                query(None, Some((12, 345))),
                json!({
                    "method": "RemoteGetHistoryPageV1",
                    "params": {
                        "project_id": PROJECT,
                        "session_id": SESSION,
                        "window": {
                            "kind": "older",
                            "anchor": { "sequence": 12, "id": "345" },
                        },
                        "limit": 25,
                        "cursor": null,
                    },
                }),
            ),
            (
                project_path("/history"),
                query(Some("h2"), Some((-1, 7))),
                json!({
                    "method": "RemoteGetHistoryPageV1",
                    "params": {
                        "project_id": PROJECT,
                        "session_id": SESSION,
                        "window": {
                            "kind": "older",
                            "anchor": { "sequence": -1, "id": "7" },
                        },
                        "limit": 25,
                        "cursor": { "kind": "history", "token": "h2" },
                    },
                }),
            ),
        ];
        for (path, query, expected) in cases {
            let request = build_request(&path, &query, &policy).expect("allowed query builds");
            assert_eq!(serde_json::to_value(&request).unwrap(), expected, "{path}");
        }
    }

    #[test]
    fn queries_not_defined_for_a_route_deny() {
        let policy = policy();
        for (path, query) in [
            ("/api/v1/info".to_string(), query(Some("tok"), None)),
            (project_path(""), query(Some("tok"), None)),
            (
                format!("/api/v1/projects/{PROJECT}/sessions"),
                query(None, Some((1, 2))),
            ),
            ("/api/v1/projects".to_string(), query(None, Some((1, 2)))),
            (project_path("/decisions"), query(None, Some((1, 2)))),
        ] {
            assert_eq!(
                build_request(&path, &query, &policy).unwrap_err(),
                ReadDenial::InvalidRequest,
                "{path}",
            );
        }
    }

    #[test]
    fn out_of_scope_project_denies_before_any_query_check() {
        let policy = policy();
        let path = format!("/api/v1/projects/{OTHER}/sessions");
        assert_eq!(
            build_request(&path, &query(Some("tok"), None), &policy).unwrap_err(),
            ReadDenial::ProjectOutOfScope,
        );
        assert_eq!(
            build_request(&path, &query(None, Some((1, 2))), &policy).unwrap_err(),
            ReadDenial::ProjectOutOfScope,
        );
    }

    #[test]
    fn out_of_scope_projects_and_unknown_routes_deny() {
        let policy = policy();
        for path in [
            format!("/api/v1/projects/{OTHER}/sessions"),
            format!("/api/v1/projects/{OTHER}/sessions/{SESSION}"),
            format!("/api/v1/projects/{OTHER}/sessions/{SESSION}/history"),
            format!("/api/v1/projects/{OTHER}/sessions/{SESSION}/decisions"),
        ] {
            assert_eq!(
                build_request(&path, &Query::default(), &policy).unwrap_err(),
                ReadDenial::ProjectOutOfScope
            );
        }
        assert_eq!(
            build_request("/api/v1/AgentHalt", &Query::default(), &policy).unwrap_err(),
            ReadDenial::NoRoute
        );
    }

    #[tokio::test]
    async fn denials_never_dispatch() {
        let policy = policy();
        let dispatch = CountingDispatch::new();
        for path in [
            "/api/v1/AgentHalt".to_string(),
            format!("/api/v1/projects/{OTHER}/sessions"),
            format!("/api/v1/projects/{OTHER}/sessions/{SESSION}"),
            format!("/api/v1/projects/{OTHER}/sessions/{SESSION}/history"),
            format!("/api/v1/projects/{OTHER}/sessions/{SESSION}/decisions"),
        ] {
            let result = serve_read(&path, &Query::default(), &policy, &dispatch).await;
            assert!(matches!(result, Err(ServeError::Denied(_))));
        }
        for (path, query) in [
            ("/api/v1/info".to_string(), query(Some("tok"), None)),
            (project_path(""), query(Some("tok"), None)),
            (
                format!("/api/v1/projects/{PROJECT}/sessions"),
                query(None, Some((1, 2))),
            ),
            (
                format!("/api/v1/projects/{OTHER}/sessions"),
                query(Some("tok"), None),
            ),
        ] {
            let result = serve_read(&path, &query, &policy, &dispatch).await;
            assert!(matches!(result, Err(ServeError::Denied(_))), "{path}");
        }
        assert_eq!(dispatch.calls(), 0);
        assert!(
            serve_read("/api/v1/info", &Query::default(), &policy, &dispatch)
                .await
                .is_ok()
        );
        assert_eq!(dispatch.calls(), 1);
    }

    fn start_fake(dir: &Path, reply: Vec<u8>) -> (PathBuf, Arc<Mutex<Option<String>>>) {
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(None));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut received = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    received.extend_from_slice(&chunk[..n]);
                    if received.contains(&b'\n') {
                        break;
                    }
                }
                *recorder.lock().unwrap() = Some(String::from_utf8_lossy(&received).to_string());
                let _ = stream.write_all(&reply).await;
                let _ = stream.flush().await;
            }
        });
        (socket, seen)
    }

    fn info_request() -> ReadRequestV1 {
        build_request("/api/v1/info", &Query::default(), &policy()).unwrap()
    }

    #[tokio::test]
    async fn dispatch_sends_one_jsonrpc_object_without_a_session_token() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, seen) = start_fake(
            dir.path(),
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n".to_vec(),
        );
        let reads = DaemonReads::new(socket);
        let body = reads.dispatch(&info_request()).await.unwrap();
        assert_eq!(body, b"{\"ok\":true}".to_vec());
        let line = seen.lock().unwrap().clone().expect("line recorded");
        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["jsonrpc"], json!("2.0"));
        assert_eq!(value["id"], json!(1));
        assert_eq!(value["method"], json!("RemoteGetInfoV1"));
        assert_eq!(value["params"], json!({}));
        assert!(value.get("session_token").is_none());
        assert!(!line.contains("session_token"));
    }

    #[tokio::test]
    async fn daemon_error_object_maps_to_daemon_code() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _) = start_fake(
            dir.path(),
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"nope\"}}\n"
                .to_vec(),
        );
        let reads = DaemonReads::new(socket);
        assert_eq!(
            reads.dispatch(&info_request()).await.unwrap_err(),
            ReadError::Daemon(-32601)
        );
    }

    #[tokio::test]
    async fn non_object_result_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, _) = start_fake(
            dir.path(),
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":42}\n".to_vec(),
        );
        let reads = DaemonReads::new(socket);
        assert_eq!(
            reads.dispatch(&info_request()).await.unwrap_err(),
            ReadError::Malformed
        );
    }

    #[tokio::test]
    async fn oversized_line_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut reply = vec![b'x'; MAX_RESPONSE_BYTES + 1];
        reply.push(b'\n');
        let (socket, _) = start_fake(dir.path(), reply);
        let reads = DaemonReads::new(socket);
        assert_eq!(
            reads.dispatch(&info_request()).await.unwrap_err(),
            ReadError::Oversize
        );
    }

    #[tokio::test]
    async fn silent_daemon_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let reads = DaemonReads::new(socket).with_deadline(Duration::from_millis(100));
        assert_eq!(
            reads.dispatch(&info_request()).await.unwrap_err(),
            ReadError::Timeout
        );
    }

    #[tokio::test]
    async fn missing_socket_is_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let reads = DaemonReads::new(dir.path().join("absent.sock"));
        assert_eq!(
            reads.dispatch(&info_request()).await.unwrap_err(),
            ReadError::Connect
        );
    }

    #[test]
    fn denial_and_error_codes_are_stable() {
        assert_eq!(ReadDenial::NoRoute.code(), "no_route");
        assert_eq!(ReadDenial::ProjectOutOfScope.code(), "project_out_of_scope");
        assert_eq!(ReadDenial::InvalidRequest.code(), "invalid_request");
        assert_eq!(ReadError::Busy.code(), "busy");
        assert_eq!(ReadError::Timeout.code(), "timeout");
        assert_eq!(ReadError::Connect.code(), "connect");
        assert_eq!(ReadError::PeerMismatch.code(), "peer_mismatch");
        assert_eq!(ReadError::Oversize.code(), "oversize");
        assert_eq!(ReadError::Malformed.code(), "malformed");
        assert_eq!(ReadError::Daemon(-1).code(), "daemon_error");
    }
}
