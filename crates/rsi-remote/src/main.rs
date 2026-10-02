use chrono::Utc;
use rsi_remote::{
    assets,
    config::{self, Config},
    gate::{Gate, REFRESH_EVERY},
    ingress::{self, Method, Request},
    localapi_client::{DEFAULT_SOCKET, LocalApiClient},
    reads::{self, DaemonReads, ReadDispatch, ServeError, default_daemon_socket},
    session::{self, Sessions},
};
use std::{
    env,
    fmt::Write as _,
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    time::timeout_at,
};

#[tokio::main]
async fn main() {
    if let Err(error) = command().await {
        eprintln!("rsi-remote: {error}");
        std::process::exit(1);
    }
}

async fn command() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [group, action, file] if group == "config" && action == "init" => {
            config::write(Path::new(file), &Config::default(), true)?;
            println!("disabled remote policy created");
        }
        [group, action, file] if group == "config" && action == "check" => {
            let policy = config::read(Path::new(file))?;
            println!(
                "valid; enabled={}; node_count={}; project_count={}",
                policy.enabled,
                policy.allowed_node_ids.len(),
                policy.project_ids.len()
            );
        }
        [group, action, file] if group == "config" && action == "show" => {
            let policy = config::read(Path::new(file))?;
            println!(
                "enabled={} canonical_host={} owner_user_id={} node_count={} project_count={}",
                policy.enabled,
                policy.canonical_host,
                policy.owner_user_id,
                policy.allowed_node_ids.len(),
                policy.project_ids.len()
            );
        }
        [group, action, file] if group == "config" && action == "disable" => {
            let mut policy = config::read(Path::new(file))?;
            policy.enabled = false;
            config::write(Path::new(file), &policy, false)?;
            println!("remote policy disabled");
        }
        [action, config_path, socket_path] if action == "run" => {
            run(Path::new(config_path), Path::new(socket_path)).await?
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: rsi-remote config init|check|show|disable <policy.toml> | run <policy.toml> <ingress.sock>",
            ));
        }
    }
    Ok(())
}

async fn run(config_path: &Path, socket_path: &Path) -> io::Result<()> {
    let policy = config::read(config_path)?;
    if !policy.enabled {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote policy disabled",
        ));
    }
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent"))?;
    let meta = fs::symlink_metadata(parent)?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "gateway must not run as root",
        ));
    }
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe ingress directory",
        ));
    }
    if fs::symlink_metadata(socket_path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "ingress socket exists",
        ));
    }
    let listener = UnixListener::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    let slots = Arc::new(Semaphore::new(32));
    let gate = Arc::new(Gate::new(LocalApiClient::new(DEFAULT_SOCKET), socket_path));
    let sessions = Arc::new(Sessions::new());
    let reads = Arc::new(DaemonReads::new(default_daemon_socket().ok_or_else(
        || io::Error::new(io::ErrorKind::NotFound, "daemon socket unknown"),
    )?));
    {
        let gate = Arc::clone(&gate);
        let sessions = Arc::clone(&sessions);
        let config_path = config_path.to_path_buf();
        tokio::spawn(async move {
            loop {
                match config::read(&config_path) {
                    Ok(policy) if policy.enabled => {
                        if gate
                            .refresh(&policy, Utc::now(), Instant::now())
                            .await
                            .is_err()
                        {
                            sessions.clear_all();
                        }
                    }
                    _ => {
                        gate.clear();
                        sessions.clear_all();
                    }
                }
                tokio::time::sleep(REFRESH_EVERY).await;
            }
        });
    }
    loop {
        let (stream, _) = listener.accept().await?;
        // Peer check is deliberately before any HTTP read or parse.
        if !trusted_peer(stream.peer_cred()?.uid()) {
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let path = config_path.to_path_buf();
        let gate = Arc::clone(&gate);
        let sessions = Arc::clone(&sessions);
        let reads = Arc::clone(&reads);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle(stream, &path, &gate, &sessions, &*reads).await;
        });
    }
}

fn trusted_peer(uid: u32) -> bool {
    uid == 0
}

/// A complete gateway response with the fixed security headers applied at
/// serialization time.
struct Response {
    status: u16,
    reason: &'static str,
    content_type: Option<&'static str>,
    csp: bool,
    set_cookies: Vec<String>,
    body: Vec<u8>,
}

impl Response {
    const fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: None,
            csp: false,
            set_cookies: Vec::new(),
            body: Vec::new(),
        }
    }

    fn json(status: u16, reason: &'static str, body: Vec<u8>) -> Self {
        Self {
            content_type: Some("application/json"),
            body,
            ..Self::new(status, reason)
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        if let Some(content_type) = self.content_type {
            let _ = write!(head, "content-type: {content_type}\r\n");
        }
        if self.csp {
            let _ = write!(head, "content-security-policy: {}\r\n", assets::CSP);
        }
        for cookie in &self.set_cookies {
            let _ = write!(head, "set-cookie: {cookie}\r\n");
        }
        let _ = write!(
            head,
            "content-length: {}\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n",
            self.body.len()
        );
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionBody {
    nonce: String,
}

/// Decide and build the response for one validated, authorized request.
///
/// The caller must have run identity authorization and produced `binding` and
/// `origin_ok`; this function owns routes, session checks and read dispatch.
#[allow(clippy::too_many_arguments)]
async fn route<D: ReadDispatch>(
    req: &Request,
    body: &[u8],
    origin_ok: bool,
    binding: &session::Binding,
    sessions: &Sessions,
    policy: &Config,
    reads: &D,
    now: Instant,
) -> Response {
    if let Some((content_type, bytes)) = assets::asset(&req.path) {
        let mut response = Response::new(200, "OK");
        response.content_type = Some(content_type);
        response.csp = true;
        response.body = bytes.to_vec();
        return response;
    }
    match (req.method, req.path.as_str()) {
        (Method::Get, "/auth/bootstrap") => match sessions.bootstrap(binding, now) {
            Ok(issued) => {
                let mut response = Response::json(200, "OK", json_string("nonce", &issued.secret));
                response
                    .set_cookies
                    .push(session::set_boot_cookie(&issued.cookie));
                response
            }
            Err(_) => Response::new(503, "Service Unavailable"),
        },
        (Method::Post, "/auth/session") => {
            let Ok(parsed) = serde_json::from_slice::<SessionBody>(body) else {
                return Response::new(400, "Bad Request");
            };
            let boot = session::cookie(req.cookie.as_deref(), session::BOOT_COOKIE);
            match sessions.establish(boot, &parsed.nonce, origin_ok, binding, now) {
                Ok(issued) => {
                    let mut response =
                        Response::json(200, "OK", json_string("csrf", &issued.secret));
                    response
                        .set_cookies
                        .push(session::set_session_cookie(&issued.cookie));
                    response
                        .set_cookies
                        .push(session::clear_cookie(session::BOOT_COOKIE));
                    response
                }
                Err(_) => Response::new(403, "Forbidden"),
            }
        }
        (Method::Post, "/auth/logout") => {
            let cookie = session::cookie(req.cookie.as_deref(), session::SESSION_COOKIE);
            match sessions.logout(cookie, req.csrf.as_deref(), origin_ok, binding, now) {
                Ok(()) => {
                    let mut response = Response::new(204, "No Content");
                    response
                        .set_cookies
                        .push(session::clear_cookie(session::SESSION_COOKIE));
                    response
                }
                Err(_) => Response::new(403, "Forbidden"),
            }
        }
        (Method::Get, path) if path.starts_with("/api/v1/") => {
            let cookie = session::cookie(req.cookie.as_deref(), session::SESSION_COOKIE);
            if sessions.check(cookie, origin_ok, binding, now).is_err() {
                return Response::new(401, "Unauthorized");
            }
            match reads::serve_read(&req.path, &req.query, policy, reads).await {
                Ok(bytes) => Response::json(200, "OK", bytes),
                Err(ServeError::Denied(_)) => Response::new(403, "Forbidden"),
                Err(ServeError::Failed(_)) => Response::new(502, "Bad Gateway"),
            }
        }
        _ => Response::new(404, "Not Found"),
    }
}

fn json_string(key: &str, value: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ key: value })).unwrap_or_default()
}

/// Read up to and including the header terminator, returning the raw bytes and
/// the offset just past the terminator.
async fn read_headers(stream: &mut UnixStream) -> io::Result<(Vec<u8>, usize)> {
    let mut raw = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            return Ok((raw, index + 4));
        }
        if raw.len() > 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended",
            ));
        }
        raw.extend_from_slice(&chunk[..n]);
    }
}

async fn read_body(stream: &mut UnixStream, have: &mut Vec<u8>, want: usize) -> io::Result<()> {
    let mut chunk = [0u8; 1024];
    while have.len() < want {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "body ended"));
        }
        have.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

async fn handle<D: ReadDispatch>(
    mut stream: UnixStream,
    config_path: &Path,
    gate: &Gate,
    sessions: &Sessions,
    reads: &D,
) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (raw, header_end) = timeout_at(deadline, read_headers(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request timeout"))??;
    // Re-read on each request: config-disable takes effect without a restart.
    let policy = config::read(config_path)
        .ok()
        .filter(|policy| policy.enabled);
    let request = policy
        .as_ref()
        .and_then(|policy| ingress::parse_request(&raw[..header_end], &policy.canonical_host).ok());
    let body = if let Some(request) = &request {
        let mut have = raw[header_end..].to_vec();
        if have.len() > request.content_length {
            // More bytes than the declared length: drop the connection.
            return Ok(());
        }
        timeout_at(
            deadline,
            read_body(&mut stream, &mut have, request.content_length),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "body timeout"))??;
        if have.len() != request.content_length {
            return Ok(());
        }
        have
    } else {
        Vec::new()
    };
    let now = Instant::now();
    let response = match (&policy, &request) {
        (Some(policy), Some(request)) => {
            match gate
                .authorize(policy, request.source, Utc::now(), now)
                .await
            {
                Ok(identity) => match gate.ready(now) {
                    None => Response::new(403, "Forbidden"),
                    Some(server) => {
                        let binding = session::Binding {
                            owner_user_id: policy.owner_user_id,
                            client_node: identity.node_stable_id,
                            server_node: server.node_id,
                        };
                        let origin_ok = session::origin_ok(
                            request.origin.as_deref(),
                            request.sec_fetch_site.as_deref(),
                            &policy.canonical_host,
                        );
                        route(
                            request, &body, origin_ok, &binding, sessions, policy, reads, now,
                        )
                        .await
                    }
                },
                Err(_) => Response::new(403, "Forbidden"),
            }
        }
        _ => Response::new(403, "Forbidden"),
    };
    stream.write_all(&response.serialize()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::remote_read::ReadRequestV1;
    use rsi_remote::reads::ReadError;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn policy() -> Config {
        Config {
            enabled: true,
            canonical_host: "host.example.ts.net".to_string(),
            owner_user_id: 501,
            allowed_node_ids: vec!["nHOME".to_string()],
            project_ids: vec![PROJECT.to_string()],
        }
    }

    fn binding() -> session::Binding {
        session::Binding {
            owner_user_id: 501,
            client_node: "nCLIENT".to_string(),
            server_node: "nHOME".to_string(),
        }
    }

    fn get(path: &str, cookie: Option<String>) -> Request {
        Request {
            method: Method::Get,
            path: path.to_string(),
            query: ingress::Query::default(),
            source: "100.101.102.103".parse().unwrap(),
            content_length: 0,
            origin: None,
            sec_fetch_site: Some("same-origin".to_string()),
            cookie,
            csrf: None,
        }
    }

    fn post(path: &str, body: &[u8], cookie: Option<String>, csrf: Option<String>) -> Request {
        Request {
            method: Method::Post,
            path: path.to_string(),
            query: ingress::Query::default(),
            source: "100.101.102.103".parse().unwrap(),
            content_length: body.len(),
            origin: Some(format!("https://{}", policy().canonical_host)),
            sec_fetch_site: None,
            cookie,
            csrf,
        }
    }

    fn cookie_value(set_cookie: &str, name: &str) -> String {
        set_cookie
            .strip_prefix(&format!("{name}="))
            .expect("cookie name")
            .split(';')
            .next()
            .unwrap()
            .to_string()
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

    #[tokio::test]
    async fn denied_session_read_is_401_without_dispatch() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        let response = route(
            &get(
                "/api/v1/info",
                Some("__Host-rsi_remote=deadbeef".to_string()),
            ),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            Instant::now(),
        )
        .await;
        assert_eq!(response.status, 401);
        assert!(response.body.is_empty());
        assert_eq!(response.content_type, None);
        assert_eq!(dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn asset_responses_carry_csp_and_security_headers() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        for path in ["/", "/app.js", "/api.js", "/state.js", "/styles.css"] {
            let response = route(
                &get(path, None),
                b"",
                true,
                &binding(),
                &sessions,
                &policy,
                &dispatch,
                Instant::now(),
            )
            .await;
            assert_eq!(response.status, 200, "{path}");
            let text = String::from_utf8(response.serialize()).unwrap();
            assert!(
                text.contains(&format!("content-security-policy: {}\r\n", assets::CSP)),
                "{path}"
            );
            assert!(text.contains("cache-control: no-store\r\n"), "{path}");
            assert!(
                text.contains("x-content-type-options: nosniff\r\n"),
                "{path}"
            );
            assert!(text.contains("referrer-policy: no-referrer\r\n"), "{path}");
        }
        assert_eq!(dispatch.calls(), 0);
        let missing = route(
            &get("/nope.js", None),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            Instant::now(),
        )
        .await;
        assert_eq!(missing.status, 404);
    }

    #[tokio::test]
    async fn bootstrap_establish_and_logout_round_trip() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        let now = Instant::now();
        let bootstrap = route(
            &get("/auth/bootstrap", None),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        assert_eq!(bootstrap.status, 200);
        let nonce: serde_json::Value = serde_json::from_slice(&bootstrap.body).unwrap();
        let nonce = nonce["nonce"].as_str().unwrap().to_string();
        let boot_cookie = cookie_value(&bootstrap.set_cookies[0], session::BOOT_COOKIE);

        let unknown = br#"{"nonce":"00","extra":true}"#;
        let bad = route(
            &post(
                "/auth/session",
                unknown,
                Some(format!("{}={boot_cookie}", session::BOOT_COOKIE)),
                None,
            ),
            unknown,
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        assert_eq!(bad.status, 400, "unknown fields deny at parse time");

        let body = format!("{{\"nonce\":\"{nonce}\"}}");
        let establish = route(
            &post(
                "/auth/session",
                body.as_bytes(),
                Some(format!("{}={boot_cookie}", session::BOOT_COOKIE)),
                None,
            ),
            body.as_bytes(),
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        assert_eq!(establish.status, 200);
        assert_eq!(establish.set_cookies.len(), 2);
        let csrf: serde_json::Value = serde_json::from_slice(&establish.body).unwrap();
        let csrf = csrf["csrf"].as_str().unwrap().to_string();
        let session_cookie = cookie_value(&establish.set_cookies[0], session::SESSION_COOKIE);

        let logout = route(
            &post(
                "/auth/logout",
                b"",
                Some(format!("{}={session_cookie}", session::SESSION_COOKIE)),
                Some(csrf),
            ),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        assert_eq!(logout.status, 204);
        assert_eq!(logout.body.len(), 0);
        assert_eq!(dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn wrong_nonce_and_csrf_deny() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        let now = Instant::now();
        let bootstrap = route(
            &get("/auth/bootstrap", None),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        let boot_cookie = cookie_value(&bootstrap.set_cookies[0], session::BOOT_COOKIE);
        let body =
            br#"{"nonce":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        let establish = route(
            &post(
                "/auth/session",
                body,
                Some(format!("{}={boot_cookie}", session::BOOT_COOKIE)),
                None,
            ),
            body,
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        assert_eq!(establish.status, 403, "wrong nonce denies");

        let logout = route(
            &post("/auth/logout", b"", None, None),
            b"",
            false,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            now,
        )
        .await;
        assert_eq!(logout.status, 403);
        assert_eq!(dispatch.calls(), 0);
    }

    #[test]
    fn peer_check_rejects_non_root_and_accepts_only_root() {
        assert!(trusted_peer(0));
        assert!(!trusted_peer(1));
        assert!(!trusted_peer(u32::MAX));
    }

    // ---- End-to-end handler tests over a real socket pair (#879 G7) ----

    const EHOST: &str = "host.example.ts.net";
    const EHOST_PORT: &str = "host.example.ts.net:443";
    const EOWNER: u64 = 12_345;
    const ESERVER_NODE: &str = "nSERVER";
    const ECLIENT_NODE: &str = "nCLIENT";
    const ESERVER_IP: &str = "100.100.100.1";
    const ECLIENT_IP: &str = "100.101.102.103";
    const ZERO_EXPIRY: &str = "0001-01-01T00:00:00Z";

    fn remote_policy(enabled: bool) -> Config {
        Config {
            enabled,
            canonical_host: EHOST.to_string(),
            owner_user_id: EOWNER,
            allowed_node_ids: vec![ECLIENT_NODE.to_string()],
            project_ids: vec![PROJECT.to_string()],
        }
    }

    fn http_ok(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn fake_localapi_body(target: &str, ingress: &Path, client_node: &str) -> String {
        if target.starts_with("/localapi/v0/status") {
            format!(
                r#"{{"BackendState":"Running","Self":{{"ID":"{ESERVER_NODE}","UserID":{EOWNER},"DNSName":"{EHOST}.","TailscaleIPs":["{ESERVER_IP}"],"Expired":false,"KeyExpiry":"{ZERO_EXPIRY}"}},"CurrentTailnet":{{"MagicDNSSuffix":"example.ts.net"}}}}"#
            )
        } else if target.starts_with("/localapi/v0/serve-config") {
            format!(
                r#"{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"{EHOST_PORT}":{{"Handlers":{{"/":{{"Proxy":"unix:{}"}}}}}}}},"AllowFunnel":{{}}}}"#,
                ingress.display()
            )
        } else if target.contains(ESERVER_IP) {
            format!(
                r#"{{"UserProfile":{{"ID":{EOWNER}}},"Node":{{"StableID":"{ESERVER_NODE}","User":{EOWNER},"MachineAuthorized":true,"Expired":false,"KeyExpiry":"{ZERO_EXPIRY}"}}}}"#
            )
        } else {
            format!(
                r#"{{"UserProfile":{{"ID":{EOWNER}}},"Node":{{"StableID":"{client_node}","User":{EOWNER},"MachineAuthorized":true,"Expired":false,"KeyExpiry":"{ZERO_EXPIRY}","Addresses":["{ECLIENT_IP}/32"],"Tags":[],"Sharer":0}}}}"#
            )
        }
    }

    /// A LocalAPI stand-in that answers `status`, `serve-config` and both
    /// `whois` variants by request path for the lifetime of the test.
    struct FakeLocalApi {
        _dir: tempfile::TempDir,
        socket: PathBuf,
    }

    impl FakeLocalApi {
        fn start(ingress: &Path, client_node: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let ingress = ingress.to_path_buf();
            let client_node = client_node.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let target = String::from_utf8_lossy(&buf)
                        .split("\r\n")
                        .next()
                        .unwrap_or("")
                        .split(' ')
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let body = fake_localapi_body(&target, &ingress, &client_node);
                    let _ = stream.write_all(&http_ok(&body)).await;
                    let _ = stream.shutdown().await;
                }
            });
            Self { _dir: dir, socket }
        }

        fn client(&self) -> LocalApiClient {
            LocalApiClient::new(&self.socket)
        }
    }

    /// A counting read dispatcher that always returns the empty project list.
    struct ItemsDispatch {
        calls: AtomicUsize,
        last: std::sync::Mutex<Option<ReadRequestV1>>,
    }

    impl ItemsDispatch {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                last: std::sync::Mutex::new(None),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last(&self) -> Option<ReadRequestV1> {
            self.last.lock().unwrap().clone()
        }
    }

    impl ReadDispatch for ItemsDispatch {
        async fn dispatch(&self, request: &ReadRequestV1) -> Result<Vec<u8>, ReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(request.clone());
            Ok(br#"{"items":[]}"#.to_vec())
        }
    }

    /// Everything a handler test shares: an on-disk policy, a refreshed gate
    /// over the fake LocalAPI, real sessions and a counting read dispatcher.
    struct Harness {
        _dir: tempfile::TempDir,
        _fake: FakeLocalApi,
        config_path: PathBuf,
        gate: Arc<Gate>,
        sessions: Arc<Sessions>,
        dispatch: Arc<ItemsDispatch>,
    }

    async fn harness(client_node: &str) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = dir.path().join("policy.toml");
        let policy = remote_policy(true);
        config::write(&config_path, &policy, true).unwrap();
        let ingress = dir.path().join("ingress.sock");
        let fake = FakeLocalApi::start(&ingress, client_node);
        let gate = Arc::new(Gate::new(fake.client(), &ingress));
        gate.refresh(&policy, Utc::now(), Instant::now())
            .await
            .unwrap();
        Harness {
            _dir: dir,
            _fake: fake,
            config_path,
            gate,
            sessions: Arc::new(Sessions::new()),
            dispatch: Arc::new(ItemsDispatch::new()),
        }
    }

    fn raw(
        method: &str,
        path: &str,
        source: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Vec<u8> {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Host: {EHOST}\r\nX-Forwarded-Proto: https\r\nX-Forwarded-For: {source}\r\n"
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn parse_response(bytes: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header terminator");
        let head = std::str::from_utf8(&bytes[..split]).unwrap();
        let body = bytes[split + 4..].to_vec();
        let mut lines = head.split("\r\n");
        let status: u16 = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .expect("status code");
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        (status, headers, body)
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn headers_all<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
        headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// Drive one raw request through the real `handle` over a socket pair.
    async fn exchange(
        harness: &Harness,
        request: Vec<u8>,
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let (mut client, server) = UnixStream::pair().unwrap();
        let config_path = harness.config_path.clone();
        let gate = Arc::clone(&harness.gate);
        let sessions = Arc::clone(&harness.sessions);
        let reads = Arc::clone(&harness.dispatch);
        let task =
            tokio::spawn(
                async move { handle(server, &config_path, &gate, &sessions, &*reads).await },
            );
        client.write_all(&request).await.unwrap();
        client.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        task.await.expect("handler task").expect("handler");
        parse_response(&bytes)
    }

    #[tokio::test]
    async fn end_to_end_bootstrap_session_read_logout() {
        let harness = harness(ECLIENT_NODE).await;

        let (status, headers, body) = exchange(
            &harness,
            raw("GET", "/auth/bootstrap", ECLIENT_IP, &[], b""),
        )
        .await;
        assert_eq!(status, 200);
        let nonce = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        let boot_cookie = headers_all(&headers, "set-cookie");
        assert_eq!(boot_cookie.len(), 1);
        assert!(boot_cookie[0].starts_with("__Host-rsi_boot="));
        let boot = cookie_value(boot_cookie[0], session::BOOT_COOKIE);

        let body = format!("{{\"nonce\":\"{nonce}\"}}").into_bytes();
        let (status, headers, body) = exchange(
            &harness,
            raw(
                "POST",
                "/auth/session",
                ECLIENT_IP,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", &body.len().to_string()),
                    ("Origin", &format!("https://{EHOST}")),
                    ("Cookie", &format!("__Host-rsi_boot={boot}")),
                ],
                &body,
            ),
        )
        .await;
        assert_eq!(status, 200);
        let csrf = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["csrf"]
            .as_str()
            .unwrap()
            .to_string();
        let set_cookies = headers_all(&headers, "set-cookie");
        let session_cookie = set_cookies
            .iter()
            .find(|cookie| cookie.starts_with("__Host-rsi_remote="))
            .expect("session cookie");
        let session_token = cookie_value(session_cookie, session::SESSION_COOKIE);

        let (status, headers, body) = exchange(
            &harness,
            raw(
                "GET",
                "/api/v1/projects",
                ECLIENT_IP,
                &[
                    ("Cookie", &format!("__Host-rsi_remote={session_token}")),
                    ("Sec-Fetch-Site", "same-origin"),
                ],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        assert_eq!(body, br#"{"items":[]}"#);
        assert_eq!(harness.dispatch.calls(), 1);

        let (status, _, body) = exchange(
            &harness,
            raw(
                "POST",
                "/auth/logout",
                ECLIENT_IP,
                &[
                    ("Cookie", &format!("__Host-rsi_remote={session_token}")),
                    ("Origin", &format!("https://{EHOST}")),
                    ("x-rsi-csrf", &csrf),
                    ("Content-Length", "0"),
                ],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 204);
        assert!(body.is_empty());

        let (status, _, _) = exchange(
            &harness,
            raw(
                "GET",
                "/api/v1/projects",
                ECLIENT_IP,
                &[
                    ("Cookie", &format!("__Host-rsi_remote={session_token}")),
                    ("Sec-Fetch-Site", "same-origin"),
                ],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 401);
        assert_eq!(harness.dispatch.calls(), 1);
    }

    #[tokio::test]
    async fn data_read_without_session_cookie_is_401() {
        let harness = harness(ECLIENT_NODE).await;
        let (status, _, body) = exchange(
            &harness,
            raw(
                "GET",
                "/api/v1/projects",
                ECLIENT_IP,
                &[("Sec-Fetch-Site", "same-origin")],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 401);
        assert!(body.is_empty());
        assert_eq!(harness.dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn disallowed_client_node_is_403_for_all_routes() {
        let harness = harness("nOTHER").await;
        for path in ["/", "/api/v1/projects"] {
            let (status, _, _) = exchange(&harness, raw("GET", path, ECLIENT_IP, &[], b"")).await;
            assert_eq!(status, 403, "{path}");
        }
        assert_eq!(harness.dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn source_outside_tailnet_is_403() {
        let harness = harness(ECLIENT_NODE).await;
        let (status, _, _) = exchange(
            &harness,
            raw("GET", "/api/v1/projects", "8.8.8.8", &[], b""),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(harness.dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn allowed_device_gets_the_ui_with_csp() {
        let harness = harness(ECLIENT_NODE).await;
        let (status, headers, body) =
            exchange(&harness, raw("GET", "/", ECLIENT_IP, &[], b"")).await;
        assert_eq!(status, 200);
        assert!(
            header(&headers, "content-type")
                .unwrap_or_default()
                .starts_with("text/html")
        );
        assert_eq!(
            header(&headers, "content-security-policy"),
            Some(assets::CSP)
        );
        assert!(!body.is_empty());
    }

    /// Establish a real session and return its cookie value, mirroring the
    /// bootstrap/session exchange the other end-to-end tests inline.
    async fn establish_session(harness: &Harness) -> String {
        let (_, headers, body) =
            exchange(harness, raw("GET", "/auth/bootstrap", ECLIENT_IP, &[], b"")).await;
        let nonce = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        let boot = cookie_value(headers_all(&headers, "set-cookie")[0], session::BOOT_COOKIE);
        let body = format!("{{\"nonce\":\"{nonce}\"}}").into_bytes();
        let (status, headers, _) = exchange(
            harness,
            raw(
                "POST",
                "/auth/session",
                ECLIENT_IP,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", &body.len().to_string()),
                    ("Origin", &format!("https://{EHOST}")),
                    ("Cookie", &format!("__Host-rsi_boot={boot}")),
                ],
                &body,
            ),
        )
        .await;
        assert_eq!(status, 200);
        let session_cookie = headers_all(&headers, "set-cookie")
            .into_iter()
            .find(|cookie| cookie.starts_with("__Host-rsi_remote="))
            .expect("session cookie")
            .to_string();
        cookie_value(&session_cookie, session::SESSION_COOKIE)
    }

    #[tokio::test]
    async fn paged_read_reaches_dispatch_and_bad_query_is_403() {
        let harness = harness(ECLIENT_NODE).await;
        let session_token = establish_session(&harness).await;
        let cookie = format!("__Host-rsi_remote={session_token}");

        let path = format!("/api/v1/projects/{PROJECT}/sessions?cursor=tok");
        let (status, _, body) = exchange(
            &harness,
            raw(
                "GET",
                &path,
                ECLIENT_IP,
                &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, br#"{"items":[]}"#);
        assert_eq!(harness.dispatch.calls(), 1);
        let seen = serde_json::to_value(harness.dispatch.last().unwrap()).unwrap();
        assert_eq!(
            seen["params"]["cursor"],
            serde_json::json!({ "kind": "sessions", "token": "tok" })
        );

        let (status, _, _) = exchange(
            &harness,
            raw(
                "GET",
                "/api/v1/projects?x=1",
                ECLIENT_IP,
                &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(harness.dispatch.calls(), 1, "denial never dispatches");
    }

    #[tokio::test]
    async fn disabled_policy_is_403_for_every_route() {
        let harness = harness(ECLIENT_NODE).await;
        config::write(&harness.config_path, &remote_policy(false), false).unwrap();
        for path in ["/", "/api/v1/projects", "/auth/bootstrap"] {
            let (status, _, _) = exchange(&harness, raw("GET", path, ECLIENT_IP, &[], b"")).await;
            assert_eq!(status, 403, "{path}");
        }
        assert_eq!(harness.dispatch.calls(), 0);
    }
}
