use chrono::Utc;
use rsi_remote::{
    answers::{self, AnswerGate, AnswerRequest, FileAudit},
    assets,
    config::{self, Config},
    gate::{Gate, REFRESH_EVERY},
    ingress::{self, Method, Request},
    localapi_client::{DEFAULT_SOCKET, LocalApiClient},
    managers::{self, MessageRequest},
    reads::{
        self, DaemonReads, OperatorDispatch, ReadDispatch, ReadError, ServeError,
        default_daemon_socket,
    },
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
                "enabled={} canonical_host={} owner_user_id={} node_count={} project_count={} allow_answers={} allow_messages={}",
                policy.enabled,
                policy.canonical_host,
                policy.owner_user_id,
                policy.allowed_node_ids.len(),
                policy.project_ids.len(),
                policy.allow_answers,
                policy.allow_messages
            );
        }
        [group, action, file] if group == "config" && action == "disable" => {
            let mut policy = config::read(Path::new(file))?;
            policy.enabled = false;
            config::write(Path::new(file), &policy, false)?;
            println!("remote policy disabled");
        }
        [group, action, file, state] if group == "config" && action == "answers" => {
            let mut policy = config::read(Path::new(file))?;
            match state.as_str() {
                "on" => policy.allow_answers = true,
                "off" => policy.allow_answers = false,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "usage: rsi-remote config answers <policy.toml> on|off",
                    ));
                }
            }
            config::write(Path::new(file), &policy, false)?;
            println!(
                "answers {}",
                if policy.allow_answers { "on" } else { "off" }
            );
        }
        [group, action, file, state] if group == "config" && action == "messages" => {
            let mut policy = config::read(Path::new(file))?;
            match state.as_str() {
                "on" => policy.allow_messages = true,
                "off" => policy.allow_messages = false,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "usage: rsi-remote config messages <policy.toml> on|off",
                    ));
                }
            }
            config::write(Path::new(file), &policy, false)?;
            println!(
                "messages {}",
                if policy.allow_messages { "on" } else { "off" }
            );
        }
        [action, config_path, socket_path] if action == "run" => {
            run(Path::new(config_path), Path::new(socket_path)).await?
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: rsi-remote config init|check|show|disable <policy.toml> | config answers <policy.toml> on|off | config messages <policy.toml> on|off | run <policy.toml> <ingress.sock>",
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
    // Opened lazily on the first answer: an unwritable audit directory makes
    // every answer refuse with 503, it never stops the read-only gateway.
    let audit_dir = FileAudit::default_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "audit state directory unknown"))?;
    let answers = Arc::new(AnswerGate::new(Box::new(FileAudit::new(audit_dir))));
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
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            answer_busy(stream);
            continue;
        };
        // Peer check is deliberately before any HTTP parse. A credential read
        // failure counts as untrusted instead of ending the accept loop.
        let trusted = peer_is_trusted(&stream);
        let path = config_path.to_path_buf();
        let gate = Arc::clone(&gate);
        let sessions = Arc::clone(&sessions);
        let answers = Arc::clone(&answers);
        let reads = Arc::clone(&reads);
        tokio::spawn(async move {
            let _permit = permit;
            let _ =
                serve_connection(stream, trusted, &path, &gate, &sessions, &answers, &*reads).await;
        });
    }
}

fn trusted_peer(uid: u32) -> bool {
    uid == 0
}

/// Answer a connection that found every slot busy, without awaiting, so the
/// accept loop never blocks. tokio's `try_write` on a just-accepted stream has
/// no write readiness cached yet and sends nothing (#1645), so the fixed 503
/// goes out with one write on the std socket (`into_std` keeps it
/// non-blocking; an empty socket buffer takes the whole answer). The request is
/// left unread, so the peer may see a reset after it has read the answer.
fn answer_busy(stream: UnixStream) {
    log_denial("busy");
    if let Ok(mut stream) = stream.into_std() {
        let _ = io::Write::write_all(
            &mut stream,
            &Response::new(503, "Service Unavailable").serialize(),
        );
    }
}

fn peer_is_trusted(stream: &UnixStream) -> bool {
    stream
        .peer_cred()
        .is_ok_and(|cred| trusted_peer(cred.uid()))
}

/// One accepted connection. An untrusted peer (anything but tailscaled as
/// root) gets the fixed 403 instead of a dropped connection: a drop reads as a
/// reset to curl and as a contentless 502 to `tailscale serve` (#1639).
async fn serve_connection<D: ReadDispatch + OperatorDispatch>(
    mut stream: UnixStream,
    trusted: bool,
    config_path: &Path,
    gate: &Gate,
    sessions: &Sessions,
    answers: &AnswerGate,
    reads: &D,
) -> io::Result<()> {
    if trusted {
        return handle(stream, config_path, gate, sessions, answers, reads).await;
    }
    log_denial("untrusted_peer");
    // Discard what the peer sent (same size and time bounds as a request,
    // never parsed) so closing does not reset it, then answer.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let head_only = matches!(
        timeout_at(deadline, read_headers(&mut stream)).await,
        Ok(Ok((raw, _))) if raw.starts_with(b"HEAD ")
    );
    stream
        .write_all(&Response::new(403, "Forbidden").serialize_for(head_only))
        .await
}

/// A complete gateway response with the fixed security headers applied at
/// serialization time.
/// Fixed body for every 403; never names which check failed.
#[cfg(test)]
const FORBIDDEN_BODY: &[u8] = b"forbidden\n";

/// Minimum spacing between log lines for the same denial code.
const DENY_LOG_EVERY: Duration = Duration::from_secs(5);

/// Per-code rate limiter for denial log lines.
#[derive(Default)]
struct DenyLimiter {
    last: std::collections::HashMap<String, Instant>,
}

impl DenyLimiter {
    /// The log line for `code`, unless the same code was logged within
    /// [`DENY_LOG_EVERY`]. Only the code is ever included.
    fn line(&mut self, code: &str, now: Instant) -> Option<String> {
        if self
            .last
            .get(code)
            .is_some_and(|at| now.duration_since(*at) < DENY_LOG_EVERY)
        {
            return None;
        }
        self.last.insert(code.to_string(), now);
        Some(format!("rsi-remote: denied: {code}"))
    }
}

/// Log a denial code (never tokens, cookies or request content) to stderr.
fn log_denial(code: &str) {
    static LIMITER: std::sync::OnceLock<std::sync::Mutex<DenyLimiter>> = std::sync::OnceLock::new();
    let line = LIMITER
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .line(code, Instant::now());
    if let Some(line) = line {
        eprintln!("{line}");
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: Option<&'static str>,
    csp: bool,
    set_cookies: Vec<String>,
    retry_after: Option<u64>,
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
            retry_after: None,
            body: Vec::new(),
        }
    }

    /// The reply of the answer pipeline, with the reason phrase of its status.
    fn from_answer(reply: answers::AnswerReply) -> Self {
        let reason = match reply.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            429 => "Too Many Requests",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Internal Server Error",
        };
        let mut response = if reply.body.is_empty() {
            Self::new(reply.status, reason)
        } else {
            Self::json(reply.status, reason, reply.body)
        };
        response.retry_after = reply.retry_after;
        response
    }

    fn json(status: u16, reason: &'static str, body: Vec<u8>) -> Self {
        Self {
            content_type: Some("application/json"),
            body,
            ..Self::new(status, reason)
        }
    }

    fn serialize(&self) -> Vec<u8> {
        self.serialize_for(false)
    }

    /// `head_only` keeps the headers (including the GET content-length) but
    /// sends no body bytes, as HTTP requires for a HEAD response.
    fn serialize_for(&self, head_only: bool) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        // A typeless empty response makes Safari download it. Every bodiless
        // status except 204 gets a fixed text body naming only the status
        // (403 is always "forbidden"); the deny reason goes to the host log.
        let blank = self.status != 204 && self.body.is_empty() && self.content_type.is_none();
        let content_type = if blank {
            Some("text/plain; charset=utf-8")
        } else {
            self.content_type
        };
        let status_text;
        let body: &[u8] = if blank {
            status_text = format!("{}\n", self.reason.to_ascii_lowercase());
            status_text.as_bytes()
        } else {
            &self.body
        };
        if let Some(content_type) = content_type {
            let _ = write!(head, "content-type: {content_type}\r\n");
        }
        if self.csp {
            let _ = write!(head, "content-security-policy: {}\r\n", assets::CSP);
        }
        for cookie in &self.set_cookies {
            let _ = write!(head, "set-cookie: {cookie}\r\n");
        }
        if let Some(seconds) = self.retry_after {
            let _ = write!(head, "retry-after: {seconds}\r\n");
        }
        let _ = write!(
            head,
            "content-length: {}\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let mut bytes = head.into_bytes();
        if !head_only {
            bytes.extend_from_slice(body);
        }
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
async fn route<D: ReadDispatch + OperatorDispatch>(
    req: &Request,
    body: &[u8],
    origin_ok: bool,
    binding: &session::Binding,
    sessions: &Sessions,
    policy: &Config,
    reads: &D,
    answers: &AnswerGate,
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
            if ingress::read_method(&req.path) == Some("RemoteGetDecisionTargetsV1") {
                // Answer routes are one opt-in: while `allow_answers` is off
                // the phone gets no digests or versions and nothing is
                // dispatched.
                if !policy.allow_answers {
                    return Response::new(403, "Forbidden");
                }
                let started = Instant::now();
                return match reads::serve_operator(&req.path, &req.query, policy, reads).await {
                    Ok(bytes) => Response::json(200, "OK", bytes),
                    Err(ServeError::Denied(_)) => Response::new(403, "Forbidden"),
                    Err(ServeError::Failed(error)) => {
                        failed_read(ingress::read_method(&req.path), error, started)
                    }
                };
            }
            let started = Instant::now();
            match reads::serve_read(&req.path, &req.query, policy, reads).await {
                Ok(bytes) => Response::json(200, "OK", bytes),
                Err(ServeError::Denied(_)) => Response::new(403, "Forbidden"),
                Err(ServeError::Failed(error)) => {
                    failed_read(ingress::read_method(&req.path), error, started)
                }
            }
        }
        (Method::Get, path) if path.starts_with("/api/v2/") => {
            // API v2 reads: the exposed projects' manager seats and one
            // seat's conversation (session, scope and one closed daemon read).
            Response::from_answer(
                managers::serve_read(req, origin_ok, binding, sessions, policy, reads, now).await,
            )
        }
        (Method::Post, path) if ingress::message_route(path).is_some() => {
            // The whole message pipeline (session, CSRF, Origin, switch, scope,
            // rate, body, seat, audit, forward) lives in `managers`.
            let input = MessageRequest {
                req,
                body,
                origin_ok,
                binding,
                sessions,
                policy,
                now,
            };
            Response::from_answer(answers.messages().serve(&input, reads).await)
        }
        (Method::Post, path) if ingress::answer_route(path).is_some() => {
            // The whole answer pipeline (session, CSRF, Origin, switch, scope,
            // rate, body, targets, audit, forward) lives in `answers`.
            let input = AnswerRequest {
                req,
                body,
                origin_ok,
                binding,
                sessions,
                policy,
                now,
            };
            Response::from_answer(answers.serve(&input, reads).await)
        }
        _ => Response::new(404, "Not Found"),
    }
}

/// Seconds a phone is asked to wait after a busy or timed-out read.
const READ_RETRY_AFTER_SECS: u64 = 2;

/// The response for a failed daemon read, with one journal line (method, code,
/// status, duration; never content, ids or tokens). A busy or timed-out read
/// is a 503 with `Retry-After`; only a real connect failure is a 502.
fn failed_read(method: Option<&str>, error: ReadError, started: Instant) -> Response {
    let status = error.http_status();
    let code = error.code();
    eprintln!(
        "rsi-remote: read failed: method={} code={code} status={status} duration_ms={}",
        method.unwrap_or("-"),
        started.elapsed().as_millis()
    );
    let reason = match status {
        503 => "Service Unavailable",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    let mut response = Response::json(status, reason, json_string("error", code));
    if status == 503 {
        response.retry_after = Some(READ_RETRY_AFTER_SECS);
    }
    response
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

async fn handle<D: ReadDispatch + OperatorDispatch>(
    mut stream: UnixStream,
    config_path: &Path,
    gate: &Gate,
    sessions: &Sessions,
    answers: &AnswerGate,
    reads: &D,
) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (raw, header_end) = match timeout_at(deadline, read_headers(&mut stream)).await {
        Ok(Ok(read)) => read,
        // Answer even a malformed or stalled request; never just drop it.
        Ok(Err(error)) => {
            let (code, response) = if error.kind() == io::ErrorKind::InvalidData {
                (
                    "headers_too_large",
                    Response::new(431, "Request Header Fields Too Large"),
                )
            } else {
                ("headers_incomplete", Response::new(400, "Bad Request"))
            };
            log_denial(code);
            return stream.write_all(&response.serialize()).await;
        }
        Err(_) => {
            log_denial("headers_timeout");
            return stream
                .write_all(&Response::new(408, "Request Timeout").serialize())
                .await;
        }
    };
    // Re-read on each request: config-disable takes effect without a restart.
    let policy = config::read(config_path)
        .ok()
        .filter(|policy| policy.enabled);
    let request = policy
        .as_ref()
        .and_then(|policy| ingress::parse_request(&raw[..header_end], &policy.canonical_host).ok());
    let body = if let Some(request) = &request {
        let mut have = raw[header_end..].to_vec();
        // More bytes than the declared length, a short body or a stalled one
        // is answered and closed, never routed and never silently dropped.
        let refused = if have.len() > request.content_length {
            Some(("body_overrun", Response::new(400, "Bad Request")))
        } else {
            match timeout_at(
                deadline,
                read_body(&mut stream, &mut have, request.content_length),
            )
            .await
            {
                Ok(Ok(())) if have.len() == request.content_length => None,
                Ok(_) => Some(("body_mismatch", Response::new(400, "Bad Request"))),
                Err(_) => Some(("body_timeout", Response::new(408, "Request Timeout"))),
            }
        };
        if let Some((code, response)) = refused {
            log_denial(code);
            return stream.write_all(&response.serialize()).await;
        }
        have
    } else {
        Vec::new()
    };
    let now = Instant::now();
    let mut deny: Option<String> = None;
    let response = match (&policy, &request) {
        (Some(policy), Some(request)) => {
            match gate
                .authorize(policy, request.source, Utc::now(), now)
                .await
            {
                Ok(identity) => match gate.ready(now) {
                    None => {
                        deny = Some("not_ready".into());
                        Response::new(403, "Forbidden")
                    }
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
                        let response = route(
                            request, &body, origin_ok, &binding, sessions, policy, reads, answers,
                            now,
                        )
                        .await;
                        if response.status == 403 {
                            deny = Some("route_forbidden".into());
                        }
                        response
                    }
                },
                Err(denial) => {
                    deny = Some(denial.code().into());
                    Response::new(403, "Forbidden")
                }
            }
        }
        (None, _) => {
            deny = Some("policy_disabled_or_unreadable".into());
            Response::new(403, "Forbidden")
        }
        (Some(policy), None) => {
            deny = Some(
                match ingress::parse_request(&raw[..header_end], &policy.canonical_host) {
                    Err(reason) => format!("bad_request: {reason}"),
                    Ok(_) => "bad_request".into(),
                },
            );
            Response::new(403, "Forbidden")
        }
    };
    if let Some(code) = deny {
        log_denial(&code);
    }
    // HEAD is never an allowed method, but its deny must still carry no body.
    let head_only = raw.starts_with(b"HEAD ");
    stream.write_all(&response.serialize_for(head_only)).await?;
    Ok(())
}

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

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
            ..Config::default()
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
        operator_calls: AtomicUsize,
    }

    impl CountingDispatch {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                operator_calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn operator_calls(&self) -> usize {
            self.operator_calls.load(Ordering::SeqCst)
        }
    }

    impl ReadDispatch for CountingDispatch {
        async fn dispatch(&self, _request: &ReadRequestV1) -> Result<Vec<u8>, ReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(b"{}".to_vec())
        }
    }

    impl reads::OperatorDispatch for CountingDispatch {
        async fn call(&self, _call: &reads::OperatorCall) -> Result<Vec<u8>, reads::OperatorError> {
            self.operator_calls.fetch_add(1, Ordering::SeqCst);
            Ok(b"{}".to_vec())
        }
    }

    #[tokio::test]
    async fn denied_session_read_is_401_without_dispatch() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        let answers = AnswerGate::new(Box::new(answers::MemoryAudit::default()));
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
            &answers,
            Instant::now(),
        )
        .await;
        assert_eq!(response.status, 401);
        assert!(response.body.is_empty());
        assert_eq!(response.content_type, None);
        assert_eq!(dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn decision_targets_without_a_session_is_401_without_operator_calls() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        let answers = AnswerGate::new(Box::new(answers::MemoryAudit::default()));
        let path = format!(
            "/api/v1/projects/{PROJECT}/sessions/550e8400-e29b-41d4-a716-446655440001/decision-targets"
        );
        let response = route(
            &get(&path, None),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            &answers,
            Instant::now(),
        )
        .await;
        assert_eq!(response.status, 401);
        assert_eq!(dispatch.calls(), 0);
        assert_eq!(dispatch.operator_calls(), 0);
    }

    #[tokio::test]
    async fn asset_responses_carry_csp_and_security_headers() {
        let policy = policy();
        let sessions = Sessions::new();
        let dispatch = CountingDispatch::new();
        let answers = AnswerGate::new(Box::new(answers::MemoryAudit::default()));
        for path in ["/", "/app.js", "/api.js", "/state.js", "/styles.css"] {
            let response = route(
                &get(path, None),
                b"",
                true,
                &binding(),
                &sessions,
                &policy,
                &dispatch,
                &answers,
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
            &answers,
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
        let answers = AnswerGate::new(Box::new(answers::MemoryAudit::default()));
        let now = Instant::now();
        let bootstrap = route(
            &get("/auth/bootstrap", None),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            &answers,
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
            &answers,
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
            &answers,
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
            &answers,
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
        let answers = AnswerGate::new(Box::new(answers::MemoryAudit::default()));
        let now = Instant::now();
        let bootstrap = route(
            &get("/auth/bootstrap", None),
            b"",
            true,
            &binding(),
            &sessions,
            &policy,
            &dispatch,
            &answers,
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
            &answers,
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
            &answers,
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
            ..Config::default()
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
                r#"{{"UserProfile":{{"ID":{EOWNER}}},"Node":{{"StableID":"{ESERVER_NODE}","User":{EOWNER},"MachineAuthorized":null,"Expired":false,"KeyExpiry":"{ZERO_EXPIRY}"}}}}"#
            )
        } else {
            format!(
                r#"{{"UserProfile":{{"ID":{EOWNER}}},"Node":{{"StableID":"{client_node}","User":{EOWNER},"MachineAuthorized":null,"Expired":false,"KeyExpiry":"{ZERO_EXPIRY}","Addresses":["{ECLIENT_IP}/32"],"Tags":[],"Sharer":0}}}}"#
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
            let dir = crate::test_support::short_tempdir().unwrap();
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

    const V2_SEAT: &str = "550e8400-e29b-41d4-a716-446655440001";
    const V2_SEATS: &str = r#"{"projects":[{"project_id":"550e8400-e29b-41d4-a716-446655440000","name":"Alpha","project_manager":{"session_id":"550e8400-e29b-41d4-a716-446655440001","role":"project_manager","status":"Running","provider":"Claude","model":null,"last_activity_at":"2026-10-17T09:30:00.000000000Z","pending_decisions":0,"pending_decisions_truncated":false},"global_manager":null}]}"#;
    const V2_DASHBOARD: &str = r#"{"observed_at":"2026-10-19T19:30:00.000000000Z","satellites":[{"label":"mac-mini","reachable":true,"build_sha":null,"last_seen_at":"2026-10-19T19:29:40.000000000Z","running_agents":3}],"satellites_truncated":false,"agents":[{"provider":"Claude","effort":"high","running":2}],"agents_total":2,"agents_truncated":false,"agents_incomplete":false}"#;
    const V2_PROVIDERS: &str = r#"{"observed_at":"2026-10-22T12:00:00.000000000Z","providers":[{"provider":"Claude","status":"allowed","binding_window":"five_hour","overage_status":"rejected","is_using_overage":false,"observed_at":"2026-10-22T12:00:00.000000000Z","stale_reason":null,"windows":[{"key":"five_hour","utilization":0.42,"resets_at":"2026-10-22T15:00:00.000000000Z","resets_at_epoch":1792681200}],"windows_reason":null,"windows_truncated":false,"spend":{"cost_1h_usd":1.25,"cost_24h_usd":9.5},"spend_reason":null,"models":[],"models_truncated":false}],"providers_truncated":false,"windows_incomplete":false}"#;
    const V2_CONVERSATION: &str = r#"{"seat":{"session_id":"550e8400-e29b-41d4-a716-446655440001","role":"project_manager","status":"Running","provider":"Claude","model":null,"last_activity_at":"2026-10-17T09:30:00.000000000Z","pending_decisions":0,"pending_decisions_truncated":false},"items":[],"next_before":null,"has_more":false,"operator_messages":[]}"#;
    const V2_RECEIPT: &str = r#"{"message_id":"550e8400-e29b-41d4-a716-44665544000a","seat_session_id":"550e8400-e29b-41d4-a716-446655440001","state":"queued","created_at":"2026-10-17T09:31:00.000000000Z","deduplicated":false}"#;

    /// A counting read dispatcher that always returns the empty project list.
    struct ItemsDispatch {
        calls: AtomicUsize,
        operator_calls: AtomicUsize,
        last: std::sync::Mutex<Option<ReadRequestV1>>,
        /// Reply to a decision-targets call.
        targets: std::sync::Mutex<Vec<u8>>,
        target_requests: std::sync::Mutex<Vec<serde_json::Value>>,
        /// Reply to an answer call.
        answer: std::sync::Mutex<Result<Vec<u8>, reads::OperatorError>>,
        /// Every answer request the daemon would have received, as wire JSON.
        answer_requests: std::sync::Mutex<Vec<serde_json::Value>>,
        /// API v2 replies, and every v2 call as `(method, params)`.
        v2_seats: std::sync::Mutex<Result<Vec<u8>, reads::OperatorError>>,
        v2_send: std::sync::Mutex<Result<Vec<u8>, reads::OperatorError>>,
        v2_calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl ItemsDispatch {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                operator_calls: AtomicUsize::new(0),
                last: std::sync::Mutex::new(None),
                targets: std::sync::Mutex::new(br#"{"items":[]}"#.to_vec()),
                target_requests: std::sync::Mutex::new(Vec::new()),
                answer: std::sync::Mutex::new(Ok(br#"{"items":[]}"#.to_vec())),
                answer_requests: std::sync::Mutex::new(Vec::new()),
                v2_seats: std::sync::Mutex::new(Ok(V2_SEATS.as_bytes().to_vec())),
                v2_send: std::sync::Mutex::new(Ok(V2_RECEIPT.as_bytes().to_vec())),
                v2_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn v2_calls(&self, method: &str) -> Vec<serde_json::Value> {
            self.v2_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| m == method)
                .map(|(_, params)| params.clone())
                .collect()
        }

        fn answer_requests(&self) -> Vec<serde_json::Value> {
            self.answer_requests.lock().unwrap().clone()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn operator_calls(&self) -> usize {
            self.operator_calls.load(Ordering::SeqCst)
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

    impl reads::OperatorDispatch for ItemsDispatch {
        async fn call(&self, call: &reads::OperatorCall) -> Result<Vec<u8>, reads::OperatorError> {
            self.operator_calls.fetch_add(1, Ordering::SeqCst);
            match call {
                reads::OperatorCall::DecisionTargets(request) => {
                    self.target_requests
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(request).unwrap());
                    Ok(self.targets.lock().unwrap().clone())
                }
                reads::OperatorCall::AnswerPending(request) => {
                    self.answer_requests
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(request).unwrap());
                    self.answer.lock().unwrap().clone()
                }
                reads::OperatorCall::Answer(request) => {
                    self.answer_requests
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(request).unwrap());
                    self.answer.lock().unwrap().clone()
                }
                reads::OperatorCall::ManagerSeats(_) => {
                    self.v2_calls
                        .lock()
                        .unwrap()
                        .push((call.method().into(), call.wire().unwrap()["params"].clone()));
                    self.v2_seats.lock().unwrap().clone()
                }
                reads::OperatorCall::ManagerConversation(_) => {
                    self.v2_calls
                        .lock()
                        .unwrap()
                        .push((call.method().into(), call.wire().unwrap()["params"].clone()));
                    Ok(V2_CONVERSATION.as_bytes().to_vec())
                }
                reads::OperatorCall::SendManagerMessage(_) => {
                    self.v2_calls
                        .lock()
                        .unwrap()
                        .push((call.method().into(), call.wire().unwrap()["params"].clone()));
                    self.v2_send.lock().unwrap().clone()
                }
                reads::OperatorCall::Dashboard(_) => {
                    self.v2_calls
                        .lock()
                        .unwrap()
                        .push((call.method().into(), call.wire().unwrap()["params"].clone()));
                    Ok(V2_DASHBOARD.as_bytes().to_vec())
                }
                reads::OperatorCall::Providers(_) => {
                    self.v2_calls
                        .lock()
                        .unwrap()
                        .push((call.method().into(), call.wire().unwrap()["params"].clone()));
                    Ok(V2_PROVIDERS.as_bytes().to_vec())
                }
            }
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
        answers: Arc<AnswerGate>,
        audit: Arc<answers::MemoryAudit>,
    }

    async fn harness(client_node: &str) -> Harness {
        let dir = crate::test_support::short_tempdir().unwrap();
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
        let audit = Arc::new(answers::MemoryAudit::default());
        Harness {
            _dir: dir,
            _fake: fake,
            config_path,
            gate,
            sessions: Arc::new(Sessions::new()),
            dispatch: Arc::new(ItemsDispatch::new()),
            answers: Arc::new(AnswerGate::new(Box::new(Arc::clone(&audit)))),
            audit,
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
        let answers = Arc::clone(&harness.answers);
        let task = tokio::spawn(async move {
            handle(server, &config_path, &gate, &sessions, &answers, &*reads).await
        });
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
        assert_eq!(body, b"unauthorized\n");
        assert_eq!(harness.dispatch.calls(), 0);
    }

    #[tokio::test]
    async fn deny_is_text_plain_not_a_blank_download() {
        let harness = harness("nOTHER").await;
        let (status, headers, body) =
            exchange(&harness, raw("GET", "/", ECLIENT_IP, &[], b"")).await;
        assert_eq!(status, 403);
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str());
        assert_eq!(content_type, Some("text/plain; charset=utf-8"));
        assert_eq!(body, FORBIDDEN_BODY);
    }

    /// Write `request`, close the write half and read the whole answer.
    async fn exchange_with<F>(request: Vec<u8>, serve: impl FnOnce(UnixStream) -> F) -> Vec<u8>
    where
        F: std::future::Future<Output = io::Result<()>> + Send + 'static,
    {
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve(server));
        client.write_all(&request).await.unwrap();
        client.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        task.await.expect("connection task").expect("connection");
        bytes
    }

    fn assert_text_answer(bytes: &[u8], want_status: u16, want_body: &[u8]) {
        let (status, headers, body) = parse_response(bytes);
        assert_eq!(status, want_status);
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(
            header(&headers, "content-length"),
            Some(want_body.len().to_string().as_str())
        );
        assert_eq!(body, want_body);
    }

    /// #1639: a caller that fails the peer check (the hub's own curl, or
    /// tailscaled seen through a private user namespace) gets the fixed 403
    /// text answer, never a reset; nothing is routed or dispatched.
    #[tokio::test]
    async fn untrusted_peer_gets_text_403_not_a_reset() {
        let harness = harness(ECLIENT_NODE).await;
        let (probe, _other) = UnixStream::pair().unwrap();
        // SAFETY: geteuid has no preconditions and does not dereference pointers.
        assert_eq!(peer_is_trusted(&probe), unsafe { libc::geteuid() } == 0);
        let config_path = harness.config_path.clone();
        let gate = Arc::clone(&harness.gate);
        let sessions = Arc::clone(&harness.sessions);
        let reads = Arc::clone(&harness.dispatch);
        let answers = Arc::clone(&harness.answers);
        let bytes = exchange_with(raw("GET", "/", ECLIENT_IP, &[], b""), |server| async move {
            serve_connection(
                server,
                false,
                &config_path,
                &gate,
                &sessions,
                &answers,
                &*reads,
            )
            .await
        })
        .await;
        assert_text_answer(&bytes, 403, b"forbidden\n");
        assert_eq!(harness.dispatch.calls(), 0);
    }

    /// #1639: every request-framing refusal answers with a status and a text
    /// body instead of closing the connection without a response.
    #[tokio::test]
    async fn framing_refusals_answer_instead_of_dropping() {
        let harness = harness(ECLIENT_NODE).await;
        let run = |request: Vec<u8>| {
            let config_path = harness.config_path.clone();
            let gate = Arc::clone(&harness.gate);
            let sessions = Arc::clone(&harness.sessions);
            let reads = Arc::clone(&harness.dispatch);
            let answers = Arc::clone(&harness.answers);
            exchange_with(request, |server| async move {
                handle(server, &config_path, &gate, &sessions, &answers, &*reads).await
            })
        };
        // Headers cut off before the blank line.
        let bytes = run(b"GET / HTTP/1.1\r\nHost: localhost\r\n".to_vec()).await;
        assert_text_answer(&bytes, 400, b"bad request\n");
        // Headers past the 16 KiB bound (all bytes consumed, so no reset).
        let bytes = run(vec![b'a'; 16 * 1024 + 512]).await;
        assert_text_answer(&bytes, 431, b"request header fields too large\n");
        // More body bytes than the declared Content-Length.
        let bytes = run(raw(
            "POST",
            "/auth/session",
            ECLIENT_IP,
            &[
                ("Content-Type", "application/json"),
                ("Content-Length", "1"),
            ],
            b"xx",
        ))
        .await;
        assert_text_answer(&bytes, 400, b"bad request\n");
        // A body shorter than the declared Content-Length.
        let bytes = run(raw(
            "POST",
            "/auth/session",
            ECLIENT_IP,
            &[
                ("Content-Type", "application/json"),
                ("Content-Length", "9"),
            ],
            b"xx",
        ))
        .await;
        assert_text_answer(&bytes, 400, b"bad request\n");
        assert_eq!(harness.dispatch.calls(), 0);
    }

    /// #1645: with every slot busy, a freshly accepted connection still gets
    /// the 503 text answer (tokio `try_write` here delivered nothing).
    #[tokio::test]
    async fn busy_gateway_answers_503_text_on_a_fresh_connection() {
        let dir = crate::test_support::short_tempdir().unwrap();
        let socket = dir.path().join("ingress.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        for _ in 0..5 {
            let mut client = UnixStream::connect(&socket).await.unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            answer_busy(stream);
            // The unread request may reset the connection after the answer;
            // keep whatever arrived before that.
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 512];
            while let Ok(n) = client.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            assert_text_answer(&bytes, 503, b"service unavailable\n");
        }
    }

    #[test]
    fn failed_reads_are_503_busy_502_unreachable_and_their_own_class() {
        let started = Instant::now();
        for (error, status, retry, code) in [
            (ReadError::Busy, 503, Some("2"), "busy"),
            (ReadError::Timeout, 503, Some("2"), "timeout"),
            (ReadError::Connect, 502, None, "connect"),
            (ReadError::PeerMismatch, 502, None, "peer_mismatch"),
            (ReadError::Malformed, 500, None, "malformed"),
            (ReadError::Oversize, 500, None, "oversize"),
            (ReadError::Daemon(-32603), 500, None, "daemon_error"),
        ] {
            let response = failed_read(Some("RemoteGetInfoV1"), error, started);
            let (got, headers, body) = parse_response(&response.serialize());
            assert_eq!(got, status, "{code}");
            assert_eq!(header(&headers, "retry-after"), retry, "{code}");
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], code);
        }
    }

    #[test]
    fn every_bodiless_status_but_204_gets_a_text_body() {
        for (status, reason) in [
            (400, "Bad Request"),
            (401, "Unauthorized"),
            (403, "Forbidden"),
            (404, "Not Found"),
            (408, "Request Timeout"),
            (502, "Bad Gateway"),
            (503, "Service Unavailable"),
        ] {
            let want = format!("{}\n", reason.to_ascii_lowercase());
            assert_text_answer(
                &Response::new(status, reason).serialize(),
                status,
                want.as_bytes(),
            );
        }
        let (status, headers, body) = parse_response(&Response::new(204, "No Content").serialize());
        assert_eq!(status, 204);
        assert_eq!(header(&headers, "content-length"), Some("0"));
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn rejected_head_gets_headers_but_no_body() {
        let harness = harness(ECLIENT_NODE).await;
        let (status, headers, body) =
            exchange(&harness, raw("HEAD", "/", ECLIENT_IP, &[], b"")).await;
        assert_eq!(status, 403);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("content-type"), Some("text/plain; charset=utf-8"));
        assert_eq!(get("content-length"), Some("10"));
        assert!(body.is_empty(), "HEAD response carries no body bytes");
    }

    #[test]
    fn deny_log_line_has_only_the_code_and_is_rate_limited() {
        let mut limiter = DenyLimiter::default();
        let t0 = Instant::now();
        assert_eq!(
            limiter.line("node_not_allowed", t0).as_deref(),
            Some("rsi-remote: denied: node_not_allowed")
        );
        assert_eq!(limiter.line("node_not_allowed", t0), None);
        assert!(limiter.line("funnel_on", t0).is_some());
        assert!(
            limiter
                .line("node_not_allowed", t0 + DENY_LOG_EVERY)
                .is_some()
        );
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
        establish_session_with_csrf(harness).await.0
    }

    /// As [`establish_session`], also returning the CSRF token.
    async fn establish_session_with_csrf(harness: &Harness) -> (String, String) {
        let (_, headers, body) =
            exchange(harness, raw("GET", "/auth/bootstrap", ECLIENT_IP, &[], b"")).await;
        let nonce = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        let boot = cookie_value(headers_all(&headers, "set-cookie")[0], session::BOOT_COOKIE);
        let body = format!("{{\"nonce\":\"{nonce}\"}}").into_bytes();
        let (status, headers, body) = exchange(
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
        let csrf = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["csrf"]
            .as_str()
            .unwrap()
            .to_string();
        (cookie_value(&session_cookie, session::SESSION_COOKIE), csrf)
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
    async fn decision_targets_need_allow_answers_and_then_reach_the_operator_call() {
        let harness = harness(ECLIENT_NODE).await;
        let session_token = establish_session(&harness).await;
        let cookie = format!("__Host-rsi_remote={session_token}");
        let path = format!(
            "/api/v1/projects/{PROJECT}/sessions/550e8400-e29b-41d4-a716-446655440001/decision-targets"
        );
        let request = || {
            raw(
                "GET",
                &path,
                ECLIENT_IP,
                &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
                b"",
            )
        };

        let (status, _, _) = exchange(&harness, request()).await;
        assert_eq!(status, 403, "answers off by default");
        assert_eq!(harness.dispatch.operator_calls(), 0);

        let mut policy = remote_policy(true);
        policy.allow_answers = true;
        config::write(&harness.config_path, &policy, false).unwrap();
        let (status, _, body) = exchange(&harness, request()).await;
        assert_eq!(status, 200);
        assert_eq!(body, br#"{"items":[]}"#);
        assert_eq!(harness.dispatch.operator_calls(), 1);
        assert_eq!(harness.dispatch.calls(), 0);

        policy.allow_answers = false;
        config::write(&harness.config_path, &policy, false).unwrap();
        let (status, _, _) = exchange(&harness, request()).await;
        assert_eq!(
            status, 403,
            "switching off takes effect on the next request"
        );
        assert_eq!(harness.dispatch.operator_calls(), 1);
    }

    #[tokio::test]
    async fn decision_targets_pending_cursor_round_trips_through_http() {
        let harness = harness(ECLIENT_NODE).await;
        let session_token = establish_session(&harness).await;
        let cookie = format!("__Host-rsi_remote={session_token}");
        let mut policy = remote_policy(true);
        policy.allow_answers = true;
        config::write(&harness.config_path, &policy, false).unwrap();
        let path = format!(
            "/api/v1/projects/{PROJECT}/sessions/550e8400-e29b-41d4-a716-446655440001/decision-targets"
        );
        let cursor = "pending-approval:550e8400-e29b-41d4-a716-446655440002";
        *harness.dispatch.targets.lock().unwrap() =
            serde_json::to_vec(&serde_json::json!({"pending_next":cursor,"truncated":true}))
                .unwrap();
        let request = |target: &str| {
            raw(
                "GET",
                target,
                ECLIENT_IP,
                &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
                b"",
            )
        };
        let (status, _, body) = exchange(&harness, request(&path)).await;
        assert_eq!(status, 200);
        let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let next = response["pending_next"].as_str().unwrap();
        *harness.dispatch.targets.lock().unwrap() =
            br#"{"pending_items":[],"truncated":false}"#.to_vec();
        let (status, _, body) =
            exchange(&harness, request(&format!("{path}?pending_after={next}"))).await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["truncated"],
            false
        );
        let calls = harness.dispatch.target_requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1]["pending_after"], cursor);
        let (status, _, _) =
            exchange(&harness, request(&format!("{path}?pending_after=invalid"))).await;
        assert_eq!(status, 403);
        assert_eq!(harness.dispatch.operator_calls(), 2);
    }

    #[tokio::test]
    async fn answer_post_end_to_end_through_the_real_handler() {
        answer_post_handler_case(false).await;
    }

    #[tokio::test]
    async fn pending_answer_post_end_to_end_through_the_real_handler() {
        answer_post_handler_case(true).await;
    }

    async fn answer_post_handler_case(pending: bool) {
        const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
        const IDEM: &str = "550e8400-e29b-41d4-a716-446655440009";
        let harness = harness(ECLIENT_NODE).await;
        let (session_token, csrf) = establish_session_with_csrf(&harness).await;
        let cookie = format!("__Host-rsi_remote={session_token}");
        *harness.dispatch.targets.lock().unwrap() = if pending { serde_json::json!({
            "manager":"not_configured", "fence":null, "items":[],
            "pending_items":[{"decision_id":format!("pending-question:{IDEM}"), "target_digest":"digest-a",
                "kind":"question", "title":"Ship?", "detail":"", "options":[], "receipt":null}],
            "truncated":false
        }) } else { serde_json::json!({
            "manager": "configured",
            "fence": {"policy_version": "2", "scope_version": "1"},
            "items": [{
                "decision_key": format!("question:{SESSION_ID}"),
                "row_version": "3",
                "target_digest": "digest-a",
                "kind": "question",
                "title": "Ship?",
                "detail": "",
                "options": [],
            }],
            "truncated": false,
        }) }
        .to_string()
        .into_bytes();
        *harness.dispatch.answer.lock().unwrap() = if pending {
            Ok(
                serde_json::json!({"receipt_key":IDEM, "state":"queued", "outcome":null})
                    .to_string()
                    .into_bytes(),
            )
        } else {
            Ok(
                br#"{"event_sequence":9,"key":"receipt-key","row_version":4,"deduplicated":false}"#
                    .to_vec(),
            )
        };
        let route = if pending { "answer-pending" } else { "answer" };
        let path = format!("/api/v1/projects/{PROJECT}/sessions/{SESSION_ID}/decisions/{route}");
        let body = if pending { serde_json::json!({
            "decision_id":format!("pending-question:{IDEM}"), "expected_target_digest":"digest-a",
            "answer":"yes, ship it", "idempotency_key":IDEM
        }) } else { serde_json::json!({
            "decision_key": format!("question:{SESSION_ID}"),
            "expected_row_version": "3",
            "target_digest": "digest-a",
            "fence": {"policy_version": "2", "scope_version": "1"},
            "answer": "yes, ship it",
            "idempotency_key": IDEM,
        }) }
        .to_string()
        .into_bytes();
        let origin = format!("https://{EHOST}");
        let post = |extra: &[(&str, &str)], path: &str, body: &[u8]| {
            let length = body.len().to_string();
            let mut headers = vec![
                ("Content-Type", "application/json"),
                ("Content-Length", length.as_str()),
                ("Cookie", cookie.as_str()),
            ];
            headers.extend_from_slice(extra);
            raw("POST", path, ECLIENT_IP, &headers, body)
        };
        let good = [("Origin", origin.as_str()), ("x-rsi-csrf", csrf.as_str())];

        // Answers are off by default: nothing is dispatched.
        let (status, _, _) = exchange(&harness, post(&good, &path, &body)).await;
        assert_eq!(status, 403);
        assert_eq!(harness.dispatch.operator_calls(), 0);

        let mut policy = remote_policy(true);
        policy.allow_answers = true;
        config::write(&harness.config_path, &policy, false).unwrap();

        // Gateway denials with answers on: still nothing dispatched or audited.
        let (status, _, _) =
            exchange(&harness, post(&[("Origin", origin.as_str())], &path, &body)).await;
        assert_eq!(status, 403, "missing csrf");
        let (status, _, _) = exchange(
            &harness,
            post(
                &[
                    ("Origin", "https://evil.example"),
                    ("x-rsi-csrf", csrf.as_str()),
                ],
                &path,
                &body,
            ),
        )
        .await;
        assert_eq!(status, 403, "foreign origin");
        let (status, _, _) = exchange(&harness, post(&good, &format!("{path}?x=1"), &body)).await;
        assert_eq!(status, 403, "query on the answer route");
        let (status, _, _) = exchange(
            &harness,
            raw(
                "POST",
                &path,
                ECLIENT_IP,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", "4097"),
                    ("Cookie", cookie.as_str()),
                    ("Origin", origin.as_str()),
                    ("x-rsi-csrf", csrf.as_str()),
                ],
                b"",
            ),
        )
        .await;
        assert_eq!(status, 403, "declared body over 4 KiB");
        assert_eq!(harness.dispatch.operator_calls(), 0);
        assert!(harness.audit.lines().is_empty());

        // Pending answers reach the daemon receipt fence directly; ordinary
        // answers first read their current target. Both forward one answer.
        let (status, headers, reply) = exchange(&harness, post(&good, &path, &body)).await;
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply).unwrap()
                [if pending { "receipt_key" } else { "key" }],
            if pending { IDEM } else { "receipt-key" }
        );
        assert_eq!(
            harness.dispatch.operator_calls(),
            if pending { 1 } else { 2 }
        );
        let sent = harness.dispatch.answer_requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["project_id"], PROJECT);
        assert_eq!(sent[0]["answer"], "yes, ship it");
        if pending {
            assert_eq!(sent[0]["session_id"], SESSION_ID);
            assert_eq!(sent[0]["decision_id"], format!("pending-question:{IDEM}"));
            assert_eq!(sent[0]["expected_target_digest"], "digest-a");
            assert_eq!(sent[0]["origin"]["kind"], "remote");
            assert_eq!(sent[0]["origin"]["client_node"], ECLIENT_NODE);
            assert!(ingress::is_canonical_uuid(
                sent[0]["origin"]["gateway_epoch"].as_str().unwrap()
            ));
        } else {
            assert_eq!(sent[0]["expected_row_version"], 3);
        }
        let audit = harness.audit.lines();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0]["phase"], "attempt");
        assert_eq!(audit[1]["result"], "receipt");
        assert!(
            !serde_json::to_string(&audit)
                .unwrap()
                .contains("yes, ship it")
        );

        // A second answer within the second is rate limited, not forwarded.
        let (status, headers, _) = exchange(&harness, post(&good, &path, &body)).await;
        assert_eq!(status, 429);
        assert_eq!(header(&headers, "retry-after"), Some("1"));
        assert_eq!(harness.dispatch.answer_requests().len(), 1);

        // Switching answers off takes effect on the next request.
        policy.allow_answers = false;
        config::write(&harness.config_path, &policy, false).unwrap();
        let (status, _, _) = exchange(&harness, post(&good, &path, &body)).await;
        assert_eq!(status, 403);
        assert_eq!(harness.dispatch.answer_requests().len(), 1);
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

    fn write_policy(harness: &Harness, allow_messages: bool) {
        let policy = Config {
            allow_messages,
            ..remote_policy(true)
        };
        config::write(&harness.config_path, &policy, false).unwrap();
    }

    fn message_request(
        session_token: &str,
        csrf: Option<&str>,
        origin: Option<&str>,
        body: &[u8],
    ) -> Vec<u8> {
        let path = format!("/api/v2/projects/{PROJECT}/managers/{V2_SEAT}/messages");
        let length = body.len().to_string();
        let cookie = format!("__Host-rsi_remote={session_token}");
        let mut headers = vec![
            ("Content-Type", "application/json"),
            ("Content-Length", length.as_str()),
            ("Cookie", cookie.as_str()),
        ];
        if let Some(csrf) = csrf {
            headers.push(("x-rsi-csrf", csrf));
        }
        if let Some(origin) = origin {
            headers.push(("Origin", origin));
        }
        raw("POST", &path, ECLIENT_IP, &headers, body)
    }

    const MESSAGE_BODY: &[u8] =
        br#"{"text":"focus on the gate","idempotency_key":"550e8400-e29b-41d4-a716-446655440009"}"#;

    /// The native client binding (contracts-v2): a non-browser client keeps the
    /// CSRF and Origin protections by sending `Origin: https://<canonical_host>`
    /// itself, taking the CSRF token from the `/auth/session` body and sending
    /// the cookies back verbatim. It never sends Fetch Metadata headers.
    #[tokio::test]
    async fn native_client_binds_with_origin_and_csrf_and_reaches_every_v2_route() {
        let harness = harness(ECLIENT_NODE).await;
        write_policy(&harness, true);
        let origin = format!("https://{EHOST}");

        // Bootstrap: no cookie, no Origin required; the nonce is the body.
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
        let boot = cookie_value(headers_all(&headers, "set-cookie")[0], session::BOOT_COOKIE);

        // Session: the Origin header is mandatory (no Fetch Metadata from a
        // native client), the boot cookie is replayed, the CSRF token is in
        // the JSON body.
        let body = format!("{{\"nonce\":\"{nonce}\"}}").into_bytes();
        let session_request = |origin: Option<&str>| {
            let length = body.len().to_string();
            let cookie = format!("__Host-rsi_boot={boot}");
            let mut headers = vec![
                ("Content-Type", "application/json"),
                ("Content-Length", length.as_str()),
                ("Cookie", cookie.as_str()),
            ];
            if let Some(origin) = origin {
                headers.push(("Origin", origin));
            }
            raw("POST", "/auth/session", ECLIENT_IP, &headers, &body)
        };
        let (status, _, _) = exchange(&harness, session_request(None)).await;
        assert_eq!(
            status, 403,
            "a session without the Origin header is refused"
        );
        // The boot cookie is single use: bootstrap again.
        let (_, headers, body2) = exchange(
            &harness,
            raw("GET", "/auth/bootstrap", ECLIENT_IP, &[], b""),
        )
        .await;
        let nonce = serde_json::from_slice::<serde_json::Value>(&body2).unwrap()["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        let boot = cookie_value(headers_all(&headers, "set-cookie")[0], session::BOOT_COOKIE);
        let body = format!("{{\"nonce\":\"{nonce}\"}}").into_bytes();
        let length = body.len().to_string();
        let (status, headers, session_body) = exchange(
            &harness,
            raw(
                "POST",
                "/auth/session",
                ECLIENT_IP,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", &length),
                    ("Cookie", &format!("__Host-rsi_boot={boot}")),
                    ("Origin", &origin),
                ],
                &body,
            ),
        )
        .await;
        assert_eq!(status, 200);
        let csrf = serde_json::from_slice::<serde_json::Value>(&session_body).unwrap()["csrf"]
            .as_str()
            .unwrap()
            .to_string();
        let token = cookie_value(
            headers_all(&headers, "set-cookie")
                .into_iter()
                .find(|cookie| cookie.starts_with("__Host-rsi_remote="))
                .unwrap(),
            session::SESSION_COOKIE,
        );
        let cookie = format!("__Host-rsi_remote={token}");

        // v2 reads with the Origin header (and no Fetch Metadata).
        let get = |path: &str, origin: Option<&str>| {
            let mut headers = vec![("Cookie", cookie.as_str())];
            if let Some(origin) = origin {
                headers.push(("Origin", origin));
            }
            raw("GET", path, ECLIENT_IP, &headers, b"")
        };
        let (status, headers, body) =
            exchange(&harness, get("/api/v2/projects", Some(&origin))).await;
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        // The daemon's seat list plus the policy's switch (hub #2988).
        let served = format!(
            "{},\"messages_enabled\":true}}",
            V2_SEATS.strip_suffix('}').unwrap()
        );
        assert_eq!(body, served.as_bytes());
        let conversation = format!("/api/v2/projects/{PROJECT}/managers/{V2_SEAT}/conversation");
        let (status, _, body) = exchange(&harness, get(&conversation, Some(&origin))).await;
        assert_eq!(status, 200);
        assert_eq!(body, V2_CONVERSATION.as_bytes());
        let older = format!("{conversation}?before=3:903");
        let (status, _, _) = exchange(&harness, get(&older, Some(&origin))).await;
        assert_eq!(status, 200);
        assert_eq!(
            harness.dispatch.v2_calls("RemoteGetManagerConversationV2")[1]["before"],
            serde_json::json!({"sequence": 3, "id": 903})
        );
        // hub #3287: the dashboard, with no parameters.
        let (status, _, body) = exchange(&harness, get("/api/v2/dashboard", Some(&origin))).await;
        assert_eq!(status, 200);
        assert_eq!(body, V2_DASHBOARD.as_bytes());
        assert_eq!(
            harness.dispatch.v2_calls("RemoteGetDashboardV2"),
            [serde_json::json!({})]
        );
        // hub #3796: the providers usage read, with no parameters.
        let (status, _, body) = exchange(&harness, get("/api/v2/providers", Some(&origin))).await;
        assert_eq!(status, 200);
        assert_eq!(body, V2_PROVIDERS.as_bytes());
        assert_eq!(
            harness.dispatch.v2_calls("RemoteGetProvidersV2"),
            [serde_json::json!({})]
        );
        // Neither an Origin nor Fetch Metadata: a read is refused.
        let before = harness.dispatch.operator_calls();
        let (status, _, _) = exchange(&harness, get("/api/v2/dashboard", None)).await;
        assert_eq!(status, 401);
        let (status, _, _) = exchange(&harness, get("/api/v2/providers", None)).await;
        assert_eq!(status, 401);
        let (status, _, _) = exchange(&harness, get("/api/v2/projects", None)).await;
        assert_eq!(status, 401);
        let (status, _, _) = exchange(
            &harness,
            get("/api/v2/projects", Some("https://evil.example")),
        )
        .await;
        assert_eq!(status, 401);
        assert_eq!(harness.dispatch.operator_calls(), before);
        // The browser path is unchanged: Fetch Metadata still satisfies a read.
        let browser = raw(
            "GET",
            "/api/v2/projects",
            ECLIENT_IP,
            &[("Cookie", &cookie), ("Sec-Fetch-Site", "same-origin")],
            b"",
        );
        assert_eq!(exchange(&harness, browser).await.0, 200);

        // The message write: Origin, session cookie and CSRF all required.
        let (status, _, _) = exchange(
            &harness,
            message_request(&token, None, Some(&origin), MESSAGE_BODY),
        )
        .await;
        assert_eq!(status, 403, "no CSRF token");
        let (status, _, _) = exchange(
            &harness,
            message_request(&token, Some(&csrf), None, MESSAGE_BODY),
        )
        .await;
        assert_eq!(status, 403, "no Origin header");
        let (status, _, _) = exchange(
            &harness,
            message_request(
                &token,
                Some(&csrf),
                Some("https://evil.example"),
                MESSAGE_BODY,
            ),
        )
        .await;
        assert_eq!(status, 403, "foreign Origin");
        assert!(
            harness
                .dispatch
                .v2_calls("RemoteSendManagerMessageV2")
                .is_empty()
        );

        let (status, headers, body) = exchange(
            &harness,
            message_request(&token, Some(&csrf), Some(&origin), MESSAGE_BODY),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        assert_eq!(body, V2_RECEIPT.as_bytes());
        assert_eq!(
            harness.dispatch.v2_calls("RemoteSendManagerMessageV2"),
            vec![serde_json::json!({
                "project_id": PROJECT, "seat_session_id": V2_SEAT,
                "text": "focus on the gate", "idempotency_key": "550e8400-e29b-41d4-a716-446655440009",
            })]
        );
        let lines = harness.audit.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["result"], "receipt");
        assert!(
            !serde_json::to_string(&lines)
                .unwrap()
                .contains("focus on the gate")
        );
    }

    #[tokio::test]
    async fn v2_message_needs_the_switch_and_an_allowed_device_like_every_other_route() {
        // A device outside allowed_node_ids gets the same fixed 403 as ever,
        // before any session exists or any daemon call is made.
        let harness_other = harness("nOTHER").await;
        let (status, _, body) = exchange(
            &harness_other,
            message_request(
                &"a".repeat(64),
                Some(&"b".repeat(64)),
                Some(&format!("https://{EHOST}")),
                MESSAGE_BODY,
            ),
        )
        .await;
        assert_eq!((status, body.as_slice()), (403, FORBIDDEN_BODY));
        let (status, _, body) = exchange(
            &harness_other,
            raw("GET", "/api/v2/projects", ECLIENT_IP, &[], b""),
        )
        .await;
        assert_eq!((status, body.as_slice()), (403, FORBIDDEN_BODY));
        assert_eq!(harness_other.dispatch.operator_calls(), 0);

        // An allowed device with the switch off: bare 403, nothing dispatched;
        // the switch is read per request, so flipping the file turns it on.
        let harness = harness(ECLIENT_NODE).await;
        let (token, csrf) = establish_session_with_csrf(&harness).await;
        let origin = format!("https://{EHOST}");
        let send = || message_request(&token, Some(&csrf), Some(&origin), MESSAGE_BODY);
        let (status, _, body) = exchange(&harness, send()).await;
        assert_eq!((status, body.as_slice()), (403, FORBIDDEN_BODY));
        assert_eq!(harness.dispatch.operator_calls(), 0);
        // The project list reports the same switch the send obeys (hub #2988).
        let session_cookie = format!("__Host-rsi_remote={token}");
        let list = || {
            raw(
                "GET",
                "/api/v2/projects",
                ECLIENT_IP,
                &[
                    ("Cookie", session_cookie.as_str()),
                    ("Origin", origin.as_str()),
                ],
                b"",
            )
        };
        let messages_enabled = |body: &[u8]| {
            serde_json::from_slice::<serde_json::Value>(body).unwrap()["messages_enabled"].clone()
        };
        let (status, _, body) = exchange(&harness, list()).await;
        assert_eq!(status, 200);
        assert_eq!(messages_enabled(&body), serde_json::json!(false));
        write_policy(&harness, true);
        let (status, _, body) = exchange(&harness, list()).await;
        assert_eq!(status, 200);
        assert_eq!(messages_enabled(&body), serde_json::json!(true));
        let (status, _, _) = exchange(&harness, send()).await;
        assert_eq!(status, 200);
        write_policy(&harness, false);
        let (status, _, body) = exchange(&harness, send()).await;
        assert_eq!((status, body.as_slice()), (403, FORBIDDEN_BODY));
        assert_eq!(
            harness
                .dispatch
                .v2_calls("RemoteSendManagerMessageV2")
                .len(),
            1
        );

        // Framing: a wrong content type never routes (body bounds: ingress tests).
        write_policy(&harness, true);
        let length = MESSAGE_BODY.len().to_string();
        let cookie = format!("__Host-rsi_remote={token}");
        let path = format!("/api/v2/projects/{PROJECT}/managers/{V2_SEAT}/messages");
        let (status, _, _) = exchange(
            &harness,
            raw(
                "POST",
                &path,
                ECLIENT_IP,
                &[
                    ("Content-Type", "text/plain"),
                    ("Content-Length", &length),
                    ("Cookie", &cookie),
                ],
                MESSAGE_BODY,
            ),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(
            harness
                .dispatch
                .v2_calls("RemoteSendManagerMessageV2")
                .len(),
            1
        );
    }
}
