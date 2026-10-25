//! The Rust client of the run-local bench socket (#3682 R4, design #3628 "Architecture and
//! transport" and "Native Harness trial and recovery"; replaces `eval/harbor/rsi_harbor/client.py`).
//!
//! The daemon binds one unix socket per run (`~/.rsi/bench/<run>/adapter.sock`) that serves five
//! methods for that run only: `BenchTaskStart`, `BenchTaskStatus`, `BenchTaskCancel`,
//! `BenchTaskFinish` and `BenchTraceRead`. The wire types are `rsi_common::bench`'s, the framing
//! is the daemon's (one JSON-RPC 2.0 request per line, one reply per line). The client holds no
//! credential and sends no agent-control token: socket custody is the capability, so the
//! socket's ownership and location are checked before connecting and there is no fallback to the
//! daemon's general socket.
//!
//! Recovery rules (design "Native Harness trial and recovery"):
//!
//! * One connection per call, so a daemon restart costs only the call in flight. A call that
//!   fails to connect, or whose reply is lost, is retried with the identical request for at most
//!   the reconnect window (60 s) and never past its caller's cap. Every method is safe to repeat:
//!   `BenchTaskStart` is deduplicated by the service on `(run, trial, params)`, so a lost Start
//!   reply is retried verbatim and never invents a second trial.
//! * Each request is bounded (60 s). The agent deadline is the *original* clock ([`AgentClock`]);
//!   reconnecting never extends it.
//! * At the deadline the client cancels (10 s window) and grades only after `BenchTaskFinish`
//!   reports the session settled. If settlement cannot be established the trial is invalid and
//!   not verified. Finish is settled before the trace is paged, and the trace has its own bound.
//! * An unknown state, an unexpected reply or a refusal this client cannot place is invalid
//!   (`invalid_plan`), never a guessed grade.
//!
//! The unix socket is `cfg(unix)`; on other targets every call returns an explicit unsupported
//! error instead of failing to compile.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use rsi_common::{
    bench::{
        BENCH_AGENT_TIMEOUT, BENCH_CANCELLED, BENCH_LAUNCH_NOT_ALLOWED,
        BENCH_RUN_BUDGET_INSUFFICIENT, BENCH_TASK_NOT_FINISHED, BenchTaskStartParams,
        BenchTaskState, BenchTaskStatusV1, BenchTraceChunkV1, BenchTraceHandle,
        BenchTraceReadParams, BenchTrialParams, DEFAULT_TRACE_PAGE_BYTES, METHOD_TASK_CANCEL,
        METHOD_TASK_FINISH, METHOD_TASK_START, METHOD_TASK_STATUS, METHOD_TRACE_READ,
    },
    bench_profile::{BENCH_INVALID_ROUTE, BENCH_ROUTE_REQUIRED},
    bench_runner::{CANCEL_SETTLE_TIMEOUT_SEC, TrialException, TrialExceptionKind},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::trial::{Clock, SystemClock};

/// The daemon's request line limit; a request over it is refused before it is sent.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// The longest reply line accepted: a 1 MiB trace page can inflate under JSON escaping.
pub const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
/// The only socket file name a run's adapter socket has.
pub const SOCKET_FILE_NAME: &str = "adapter.sock";
/// Per-request timeout (design: 60 s).
pub const REQUEST_TIMEOUT_SEC: u64 = 60;
/// Reconnect window of one call (design: 60 s).
pub const RECONNECT_WINDOW_SEC: u64 = 60;
/// How long Finish may keep answering `bench_task_not_finished` after the session was stopped
/// or ended (new runner policy constant; it never extends the agent deadline).
pub const SETTLE_WINDOW_SEC: u64 = 60;
/// Upper bound of reading one trace after settlement (new runner policy constant).
pub const TRACE_WINDOW_SEC: u64 = 300;
/// A clock that can also wait; tests inject a virtual one.
pub trait Pacer: Clock {
    fn sleep(&self, duration: Duration);
}

/// The real pacer.
#[derive(Debug, Default)]
pub struct SystemPacer(SystemClock);

impl SystemPacer {
    #[must_use]
    pub fn new() -> Self {
        Self(SystemClock::new())
    }
}

impl Clock for SystemPacer {
    fn now(&self) -> Duration {
        self.0.now()
    }
}

impl Pacer for SystemPacer {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Timing policy of the client.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub request_timeout: Duration,
    pub reconnect_window: Duration,
    pub retry_interval: Duration,
    pub cancel_window: Duration,
    pub settle_window: Duration,
    pub trace_window: Duration,
    pub poll_initial: Duration,
    pub poll_max: Duration,
    /// Page size asked of `BenchTraceRead` (the service caps it at 1 MiB).
    pub trace_page_bytes: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(REQUEST_TIMEOUT_SEC),
            reconnect_window: Duration::from_secs(RECONNECT_WINDOW_SEC),
            retry_interval: Duration::from_millis(500),
            cancel_window: Duration::from_secs(CANCEL_SETTLE_TIMEOUT_SEC),
            settle_window: Duration::from_secs(SETTLE_WINDOW_SEC),
            trace_window: Duration::from_secs(TRACE_WINDOW_SEC),
            poll_initial: Duration::from_millis(500),
            poll_max: Duration::from_secs(10),
            trace_page_bytes: DEFAULT_TRACE_PAGE_BYTES,
        }
    }
}

/// A JSON-RPC error reply, with the daemon's typed `data.code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub rpc_code: i64,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

/// Why a call produced no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The daemon answered with an error.
    Refused(Refusal),
    /// Nothing could connect within the window: the request never reached the daemon.
    Unreachable(String),
    /// The request may have been delivered but no reply arrived within the window.
    Lost(String),
    /// The reply could not be interpreted, or the request was malformed.
    Protocol(String),
    /// This platform has no unix sockets, or the socket failed its custody check.
    Unsupported(String),
}

impl CallError {
    fn describe(&self) -> String {
        match self {
            Self::Refused(r) => format!("{}: {}", r.code, r.message),
            Self::Unreachable(m) => format!("daemon unreachable: {m}"),
            Self::Lost(m) => format!("reply lost: {m}"),
            Self::Protocol(m) => format!("protocol error: {m}"),
            Self::Unsupported(m) => format!("bench_runner_unsupported: {m}"),
        }
    }
}

// ---- wire ----------------------------------------------------------------------------------

// `Unsupported` is built only by the non-unix `exchange`; the others only by the unix one.
#[allow(dead_code)]
enum WireError {
    /// The request was never written.
    Connect(String),
    /// The request may have been written; no complete reply came back.
    Lost(String),
    TooLarge,
    Unsupported(String),
}

#[cfg(unix)]
fn exchange(path: &Path, request: &[u8], timeout: Duration) -> Result<Vec<u8>, WireError> {
    exchange_with_connector(request, timeout, |remaining| {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(|e| WireError::Connect(e.to_string()))?;
        let address =
            socket2::SockAddr::unix(path).map_err(|e| WireError::Connect(e.to_string()))?;
        socket
            .connect_timeout(&address, remaining)
            .map_err(|e| WireError::Connect(e.to_string()))?;
        Ok(socket.into())
    })
}

#[cfg(unix)]
fn exchange_with_connector(
    request: &[u8],
    timeout: Duration,
    connect: impl FnOnce(Duration) -> Result<std::os::unix::net::UnixStream, WireError>,
) -> Result<Vec<u8>, WireError> {
    use std::io::{ErrorKind, Read, Write};
    use std::time::Instant;

    let end = Instant::now() + timeout;
    let mut stream = connect(timeout)?;
    let mut pending = request;
    while !pending.is_empty() {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WireError::Lost(
                "request deadline reached before write".into(),
            ));
        }
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|e| WireError::Lost(e.to_string()))?;
        match stream.write(pending) {
            Ok(0) => return Err(WireError::Lost("socket closed during write".into())),
            Ok(n) => pending = &pending[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(WireError::Lost(e.to_string())),
        }
    }
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        if Instant::now() >= end {
            return Err(WireError::Lost("reply after the request timeout".into()));
        }
        if let Some(at) = buffer.iter().position(|b| *b == b'\n') {
            buffer.truncate(at);
            return Ok(buffer);
        }
        if buffer.len() > MAX_REPLY_BYTES {
            return Err(WireError::TooLarge);
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(WireError::Lost(
                "no reply within the request timeout".into(),
            ));
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| WireError::Lost(e.to_string()))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(WireError::Lost("socket closed before a reply".into())),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(WireError::Lost(
                    "no reply within the request timeout".into(),
                ));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(WireError::Lost(e.to_string())),
        }
    }
}

#[cfg(not(unix))]
fn exchange(_path: &Path, _request: &[u8], _timeout: Duration) -> Result<Vec<u8>, WireError> {
    Err(WireError::Unsupported(
        "the bench adapter socket needs unix sockets".into(),
    ))
}

/// Check the run's adapter socket before connecting: an absolute path to a socket file named
/// [`SOCKET_FILE_NAME`], not a symlink, closed to group and others, in a private directory with
/// the same owner. A daemon or other socket never passes.
///
/// # Errors
/// [`CallError::Unsupported`] naming the failed check (on non-unix targets, always).
pub fn validate_socket(path: &Path) -> Result<(), CallError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let refuse = |why: String| CallError::Unsupported(format!("bench socket {path:?}: {why}"));
        if !path.is_absolute() {
            return Err(refuse("path is not absolute".into()));
        }
        if path.file_name().and_then(|n| n.to_str()) != Some(SOCKET_FILE_NAME) {
            return Err(refuse(format!("file name is not {SOCKET_FILE_NAME}")));
        }
        let meta = std::fs::symlink_metadata(path).map_err(|e| refuse(e.to_string()))?;
        if !meta.file_type().is_socket() {
            return Err(refuse("not a unix socket".into()));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(refuse("open to group or others".into()));
        }
        let dir = path
            .parent()
            .ok_or_else(|| refuse("no parent directory".into()))?;
        let dir_meta = std::fs::symlink_metadata(dir).map_err(|e| refuse(e.to_string()))?;
        if !dir_meta.file_type().is_dir() || dir_meta.mode() & 0o077 != 0 {
            return Err(refuse("directory is not private".into()));
        }
        if dir_meta.uid() != meta.uid() {
            return Err(refuse("socket and directory have different owners".into()));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CallError::Unsupported(
            "the bench adapter socket needs unix sockets".into(),
        ))
    }
}

// ---- client --------------------------------------------------------------------------------

/// The five-method client of one run's socket.
#[derive(Debug)]
pub struct BenchClient {
    socket: PathBuf,
    config: ClientConfig,
    next_id: AtomicU64,
}

impl BenchClient {
    /// Open the client for `socket` after its custody check.
    ///
    /// # Errors
    /// [`validate_socket`]'s refusal.
    pub fn open(socket: &Path, config: ClientConfig) -> Result<Self, CallError> {
        validate_socket(socket)?;
        Ok(Self {
            socket: socket.to_path_buf(),
            config,
            next_id: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub const fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// One method call. Retries the identical request while the connection fails or the reply is
    /// lost, until `reconnect_window` or the caller's absolute `cap` (pacer time), whichever is
    /// first. The request carries no token and no run id.
    ///
    /// # Errors
    /// [`CallError`].
    fn call(
        &self,
        pacer: &dyn Pacer,
        method: &str,
        params: Value,
        cap: Option<Duration>,
    ) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }))
        .map_err(|e| CallError::Protocol(e.to_string()))?;
        line.push(b'\n');
        if line.len() > MAX_LINE_BYTES {
            return Err(CallError::Protocol(format!(
                "{method}: request exceeds the {MAX_LINE_BYTES} byte line limit"
            )));
        }
        let mut window_end = pacer.now() + self.config.reconnect_window;
        if let Some(cap) = cap {
            window_end = window_end.min(cap);
        }
        let mut maybe_delivered = false;
        loop {
            let remaining = window_end.saturating_sub(pacer.now());
            if remaining.is_zero() {
                let why = format!("{method}: call deadline reached");
                return Err(if maybe_delivered {
                    CallError::Lost(why)
                } else {
                    CallError::Unreachable(why)
                });
            }
            // One attempt never outlives the window or the caller's cap (#3843): the wait is the
            // request timeout or what is left of the window, whichever is shorter.
            let attempt = self.config.request_timeout.min(remaining);
            let failure = match exchange(&self.socket, &line, attempt) {
                Ok(reply) if pacer.now() < window_end => return decode(method, id, &reply),
                Ok(_) => {
                    return Err(CallError::Lost(format!(
                        "{method}: reply after call deadline"
                    )));
                }
                Err(WireError::Unsupported(why)) => return Err(CallError::Unsupported(why)),
                Err(WireError::TooLarge) => {
                    return Err(CallError::Protocol(format!(
                        "{method}: reply line too large"
                    )));
                }
                Err(WireError::Connect(why)) => why,
                Err(WireError::Lost(why)) => {
                    maybe_delivered = true;
                    why
                }
            };
            let now = pacer.now();
            if now >= window_end {
                return Err(if maybe_delivered {
                    CallError::Lost(failure)
                } else {
                    CallError::Unreachable(failure)
                });
            }
            pacer.sleep(self.config.retry_interval.min(window_end - now));
        }
    }

    fn status_call(
        &self,
        pacer: &dyn Pacer,
        method: &str,
        params: Value,
        trial: &str,
        cap: Option<Duration>,
    ) -> Result<BenchTaskStatusV1, CallError> {
        let result = self.call(pacer, method, params, cap)?;
        let status: BenchTaskStatusV1 = serde_json::from_value(result)
            .map_err(|e| CallError::Protocol(format!("{method}: result is not a status: {e}")))?;
        if status.trial != trial {
            return Err(CallError::Protocol(format!(
                "{method}: answered trial {:?}, asked {trial:?}",
                status.trial
            )));
        }
        Ok(status)
    }

    /// `BenchTaskStart`: idempotent per `(run, trial, params)`.
    ///
    /// # Errors
    /// [`CallError`]; the shape check of the params comes first.
    pub fn start(
        &self,
        pacer: &dyn Pacer,
        params: &BenchTaskStartParams,
        cap: Option<Duration>,
    ) -> Result<BenchTaskStatusV1, CallError> {
        params
            .validate()
            .map_err(|why| CallError::Protocol(format!("bench_invalid_request:{why}")))?;
        let value = serde_json::to_value(params).map_err(|e| CallError::Protocol(e.to_string()))?;
        self.status_call(pacer, METHOD_TASK_START, value, &params.trial, cap)
    }

    fn trial_params(trial: &str) -> Value {
        json!(BenchTrialParams {
            trial: trial.to_owned()
        })
    }

    /// `BenchTaskStatus`.
    ///
    /// # Errors
    /// [`CallError`].
    pub fn status(
        &self,
        pacer: &dyn Pacer,
        trial: &str,
        cap: Option<Duration>,
    ) -> Result<BenchTaskStatusV1, CallError> {
        self.status_call(
            pacer,
            METHOD_TASK_STATUS,
            Self::trial_params(trial),
            trial,
            cap,
        )
    }

    /// `BenchTaskCancel` within `window`: usable with only the journaled trial id, so an unknown
    /// Start outcome can still be cancelled.
    ///
    /// # Errors
    /// [`CallError`].
    pub fn cancel(
        &self,
        pacer: &dyn Pacer,
        trial: &str,
        window: Duration,
    ) -> Result<BenchTaskStatusV1, CallError> {
        let cap = pacer.now() + window;
        self.status_call(
            pacer,
            METHOD_TASK_CANCEL,
            Self::trial_params(trial),
            trial,
            Some(cap),
        )
    }

    /// `BenchTaskFinish`.
    ///
    /// # Errors
    /// [`CallError`]; `bench_task_not_finished` arrives as a retryable [`Refusal`].
    pub fn finish(
        &self,
        pacer: &dyn Pacer,
        trial: &str,
        cap: Option<Duration>,
    ) -> Result<BenchTaskStatusV1, CallError> {
        self.status_call(
            pacer,
            METHOD_TASK_FINISH,
            Self::trial_params(trial),
            trial,
            cap,
        )
    }

    /// `BenchTraceRead`: one page from `offset`.
    ///
    /// # Errors
    /// [`CallError`].
    pub fn trace_page(
        &self,
        pacer: &dyn Pacer,
        trial: &str,
        offset: u64,
        cap: Option<Duration>,
    ) -> Result<BenchTraceChunkV1, CallError> {
        let params = BenchTraceReadParams {
            trial: trial.to_owned(),
            offset,
            limit_bytes: Some(self.config.trace_page_bytes),
        };
        let value =
            serde_json::to_value(&params).map_err(|e| CallError::Protocol(e.to_string()))?;
        let result = self.call(pacer, METHOD_TRACE_READ, value, cap)?;
        serde_json::from_value(result)
            .map_err(|e| CallError::Protocol(format!("{METHOD_TRACE_READ}: not a chunk: {e}")))
    }

    /// The whole trace, page by page, checked against the handle Finish returned: its byte count
    /// and sha256 must match, so a truncated or altered trace is never reported as complete.
    ///
    /// # Errors
    /// [`CallError`], or [`CallError::Protocol`] when the pages do not add up to the handle.
    pub fn read_trace(
        &self,
        pacer: &dyn Pacer,
        trial: &str,
        handle: &BenchTraceHandle,
    ) -> Result<String, CallError> {
        let cap = pacer.now() + self.config.trace_window;
        let mut trace = String::new();
        let mut offset = 0_u64;
        loop {
            if pacer.now() >= cap {
                return Err(CallError::Lost("trace read ran past its window".into()));
            }
            let page = self.trace_page(pacer, trial, offset, Some(cap))?;
            if page.offset != offset || page.next_offset < offset {
                return Err(CallError::Protocol(format!(
                    "BenchTraceRead: page at {} answered a read at {offset}",
                    page.offset
                )));
            }
            if page.data.len() as u64 != page.next_offset - offset {
                return Err(CallError::Protocol(
                    "BenchTraceRead: page length does not match its offsets".into(),
                ));
            }
            trace.push_str(&page.data);
            if page.eof {
                break;
            }
            if page.next_offset == offset {
                return Err(CallError::Protocol(
                    "BenchTraceRead: offset did not advance".into(),
                ));
            }
            offset = page.next_offset;
        }
        if trace.len() as u64 != handle.bytes {
            return Err(CallError::Protocol(format!(
                "trace is {} bytes, the handle says {}",
                trace.len(),
                handle.bytes
            )));
        }
        let Some(expected) = handle.sha256.as_deref() else {
            return Err(CallError::Protocol(
                "the daemon recorded no trace digest, so the trace cannot be verified".into(),
            ));
        };
        let actual = trace_sha256(trace.as_bytes());
        if actual != expected.strip_prefix("sha256:").unwrap_or(expected) {
            return Err(CallError::Protocol(format!(
                "trace digest {actual} differs from the recorded {expected}"
            )));
        }
        Ok(trace)
    }
}

/// Lowercase hex sha256, the daemon's spelling of a trace digest (`bench_tasks.trace_sha256`).
#[must_use]
pub fn trace_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn decode(method: &str, id: u64, reply: &[u8]) -> Result<Value, CallError> {
    let value: Value = serde_json::from_slice(reply)
        .map_err(|e| CallError::Protocol(format!("{method}: reply is not JSON: {e}")))?;
    let Some(object) = value.as_object() else {
        return Err(CallError::Protocol(format!(
            "{method}: reply is not an object"
        )));
    };
    if object.get("id").and_then(Value::as_u64) != Some(id)
        && !object.get("id").is_some_and(Value::is_null)
    {
        return Err(CallError::Protocol(format!(
            "{method}: reply id does not match"
        )));
    }
    if let Some(error) = object.get("error").filter(|e| !e.is_null()) {
        let data = error.get("data");
        let code = data
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return Err(CallError::Refused(Refusal {
            rpc_code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            code: if code.is_empty() {
                message.clone()
            } else {
                code
            },
            message,
            retryable: data
                .and_then(|d| d.get("retryable"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }));
    }
    object
        .get("result")
        .cloned()
        .ok_or_else(|| CallError::Protocol(format!("{method}: reply has neither result nor error")))
}

// ---- one Harness trial ---------------------------------------------------------------------

/// The agent's own clock: started once, never extended by reconnecting. A resumed trial passes
/// its journaled start (R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentClock {
    /// Pacer time at which the agent phase began.
    pub started: Duration,
    pub timeout: Duration,
}

impl AgentClock {
    #[must_use]
    pub fn deadline(&self) -> Duration {
        self.started + self.timeout
    }
}

/// What reading the trace produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceOutcome {
    /// The full trace, byte count and digest checked against the daemon's handle.
    Complete { jsonl: String, sha256: String },
    /// Explicitly unavailable; no hash is fabricated.
    Unavailable { reason: String },
}

/// The result of driving one Harness trial to a settled or invalid end.
#[derive(Debug, Clone)]
pub struct HarnessTrialResult {
    pub trial: String,
    /// Whether the session is settled and the container may be verified. False for every
    /// invalid outcome, including an unknown state.
    pub verify: bool,
    /// Typed causes in order. `agent_timeout` with `verify` is the stop reason of a graded trial.
    pub exceptions: Vec<TrialException>,
    /// The final status from Finish (usage, route verdict, stop reason, trace handle), or the
    /// last status seen when Finish was never reached.
    pub status: Option<BenchTaskStatusV1>,
    pub trace: TraceOutcome,
    /// A Cancel was issued (deadline, unknown Start outcome or lost link).
    pub cancel_requested: bool,
    pub agent_elapsed_ms: u64,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn exception(kind: TrialExceptionKind, message: impl Into<String>) -> TrialException {
    TrialException {
        kind,
        message: message.into(),
    }
}

/// Place a refusal: route and spend refusals are never retried; anything else is a protocol or
/// configuration error.
fn refusal_kind(refusal: &Refusal) -> TrialExceptionKind {
    let code = refusal.code.to_ascii_lowercase();
    // Spend is placed first: a provider-named spend code (`openrouter_spend_cap_…`) must not
    // read as a route code.
    if code == BENCH_RUN_BUDGET_INSUFFICIENT || code.contains("spend") || code.contains("budget") {
        TrialExceptionKind::SpendCapRejected
    } else if code == BENCH_LAUNCH_NOT_ALLOWED
        || code == BENCH_ROUTE_REQUIRED
        || code.starts_with("bench_route")
        || code.contains("allowed_launches")
    {
        TrialExceptionKind::RouteRejected
    } else {
        TrialExceptionKind::InvalidPlan
    }
}

struct Driver<'a> {
    client: &'a BenchClient,
    pacer: &'a dyn Pacer,
    trial: &'a str,
    clock: AgentClock,
    result: HarnessTrialResult,
}

impl Driver<'_> {
    fn fail(&mut self, kind: TrialExceptionKind, message: impl Into<String>) {
        self.result.verify = false;
        self.result.exceptions.push(exception(kind, message));
    }

    fn fail_call(&mut self, what: &str, error: &CallError) {
        let kind = match error {
            CallError::Refused(refusal) => refusal_kind(refusal),
            CallError::Unreachable(_) | CallError::Lost(_) => TrialExceptionKind::InfraInterrupted,
            CallError::Protocol(_) | CallError::Unsupported(_) => TrialExceptionKind::InvalidPlan,
        };
        self.fail(kind, format!("{what}: {}", error.describe()));
    }

    /// Stop the session if one may exist. Best effort within the cancel window.
    fn cancel(&mut self) -> Result<BenchTaskStatusV1, CallError> {
        self.result.cancel_requested = true;
        let window = self.client.config.cancel_window;
        self.client.cancel(self.pacer, self.trial, window)
    }

    fn cancel_quietly(&mut self) {
        if let Err(error) = self.cancel() {
            // An unknown trial means no session was ever created; nothing more to stop.
            if !matches!(&error, CallError::Refused(r) if r.code == rsi_common::bench::BENCH_TRIAL_UNKNOWN)
            {
                let message = format!("cancel failed: {}", error.describe());
                self.result
                    .exceptions
                    .push(exception(TrialExceptionKind::InfraInterrupted, message));
            }
        }
    }

    fn elapsed(&self) -> u64 {
        millis(self.pacer.now().saturating_sub(self.clock.started))
    }

    /// Start, then poll until the session stops being live or the original agent deadline
    /// passes. `Ok(true)` when the deadline passed while it was still live.
    fn run_agent(&mut self, start: &BenchTaskStartParams) -> Result<bool, ()> {
        let deadline = self.clock.deadline();
        if self
            .result
            .status
            .as_ref()
            .is_some_and(|s| !s.state.is_live())
        {
            return Ok(false);
        }
        if self.pacer.now() >= deadline {
            // A resumed trial whose original clock expired must be cancelled by its durable
            // identity, without starting a fresh session or issuing an already-expired call.
            return Ok(true);
        }
        if self.result.status.is_none() {
            match self.client.start(self.pacer, start, Some(deadline)) {
                Ok(status) => self.result.status = Some(status),
                Err(error) => {
                    // A refusal or an unreachable daemon created no session. A lost reply may have:
                    // the journaled trial id still lets us stop it.
                    if matches!(error, CallError::Lost(_) | CallError::Protocol(_)) {
                        self.cancel_quietly();
                    }
                    self.fail_call("BenchTaskStart", &error);
                    return Err(());
                }
            }
        }
        let mut delay = self.client.config.poll_initial;
        loop {
            if self
                .result
                .status
                .as_ref()
                .is_some_and(|s| !s.state.is_live())
            {
                return Ok(false);
            }
            let now = self.pacer.now();
            if now >= deadline {
                return Ok(true);
            }
            self.pacer.sleep(delay.min(deadline - now));
            delay = delay.mul_f64(1.5).min(self.client.config.poll_max);
            match self.client.status(self.pacer, self.trial, Some(deadline)) {
                Ok(status) => self.result.status = Some(status),
                Err(error @ (CallError::Unreachable(_) | CallError::Lost(_)))
                    if self.pacer.now() < deadline =>
                {
                    // The reconnect window ran out before the agent deadline: the link is gone.
                    self.cancel_quietly();
                    self.fail_call("BenchTaskStatus", &error);
                    return Err(());
                }
                Err(CallError::Unreachable(_) | CallError::Lost(_)) => return Ok(true),
                Err(error) => {
                    self.cancel_quietly();
                    self.fail_call("BenchTaskStatus", &error);
                    return Err(());
                }
            }
        }
    }

    /// Finish until the session is settled (`bench_task_not_finished` is retryable).
    fn settle(&mut self) -> Result<BenchTaskStatusV1, ()> {
        let cap = self.pacer.now() + self.client.config.settle_window;
        let mut delay = self.client.config.poll_initial;
        loop {
            if self.pacer.now() >= cap {
                self.fail(
                    TrialExceptionKind::AgentSettleFailed,
                    format!(
                        "the session did not settle within {:?}",
                        self.client.config.settle_window
                    ),
                );
                return Err(());
            }
            match self.client.finish(self.pacer, self.trial, Some(cap)) {
                Ok(status) => return Ok(status),
                Err(CallError::Refused(r)) if r.code == BENCH_TASK_NOT_FINISHED => {
                    let now = self.pacer.now();
                    if now >= cap {
                        self.fail(
                            TrialExceptionKind::AgentSettleFailed,
                            format!(
                                "the session did not settle within {:?}",
                                self.client.config.settle_window
                            ),
                        );
                        return Err(());
                    }
                    self.pacer.sleep(delay.min(cap - now));
                    delay = delay.mul_f64(1.5).min(self.client.config.poll_max);
                }
                Err(error) => {
                    self.fail_call("BenchTaskFinish", &error);
                    return Err(());
                }
            }
        }
    }

    fn classify_settled(&mut self, status: &BenchTaskStatusV1) {
        let stop = status.stop_reason.as_deref();
        match status.state {
            BenchTaskState::Starting | BenchTaskState::Running => self.fail(
                TrialExceptionKind::InvalidPlan,
                format!("Finish returned a live state {:?}", status.state.as_str()),
            ),
            BenchTaskState::InfraInterrupted => self.fail(
                TrialExceptionKind::InfraInterrupted,
                "the daemon restarted during the trial; the session is not resumed",
            ),
            BenchTaskState::Cancelled if stop == Some(BENCH_AGENT_TIMEOUT) => {
                // The daemon's own timeout watcher got there first: still the agent deadline.
                if !self
                    .result
                    .exceptions
                    .iter()
                    .any(|e| e.kind == TrialExceptionKind::AgentTimeout)
                {
                    self.result.exceptions.push(exception(
                        TrialExceptionKind::AgentTimeout,
                        "the daemon ended the trial at its agent timeout",
                    ));
                }
            }
            BenchTaskState::Cancelled if self.result.cancel_requested => {}
            BenchTaskState::Cancelled => self.fail(
                TrialExceptionKind::InvalidPlan,
                format!(
                    "the trial was cancelled outside this runner ({})",
                    stop.unwrap_or(BENCH_CANCELLED)
                ),
            ),
            BenchTaskState::Finished => {}
        }
        if status.route_verdict.as_deref() == Some(BENCH_INVALID_ROUTE) {
            self.fail(
                TrialExceptionKind::RouteRejected,
                format!("invalid route: {}", status.route_violations.join("; ")),
            );
        }
    }
}

/// Drive one Harness trial through the run-local socket: Start, bounded polling on the original
/// agent clock, cancel-and-settle at the deadline, Finish, then the full trace.
///
/// Never panics and never grades by itself: `verify` says whether the container may be verified.
/// `clock` is `None` for a fresh trial (the agent clock starts now); a resumed trial passes its
/// original one.
pub fn drive_trial(
    client: &BenchClient,
    pacer: &dyn Pacer,
    start: &BenchTaskStartParams,
    clock: Option<AgentClock>,
) -> HarnessTrialResult {
    resume_trial(client, pacer, start, clock, None)
}

/// Continue a journaled session without replaying Start or extending its clock.
pub fn resume_trial(
    client: &BenchClient,
    pacer: &dyn Pacer,
    start: &BenchTaskStartParams,
    clock: Option<AgentClock>,
    status: Option<BenchTaskStatusV1>,
) -> HarnessTrialResult {
    let clock = clock.unwrap_or(AgentClock {
        started: pacer.now(),
        timeout: Duration::from_secs(start.agent_timeout_sec),
    });
    let mut driver = Driver {
        client,
        pacer,
        trial: &start.trial,
        clock,
        result: HarnessTrialResult {
            trial: start.trial.clone(),
            verify: true,
            exceptions: Vec::new(),
            status,
            trace: TraceOutcome::Unavailable {
                reason: "the trace was not read".into(),
            },
            cancel_requested: false,
            agent_elapsed_ms: 0,
        },
    };
    let outcome = driver.run_agent(start);
    driver.result.agent_elapsed_ms = driver.elapsed();
    match outcome {
        Err(()) => {
            driver.result.verify = false;
            return driver.result;
        }
        Ok(true) => {
            driver.result.exceptions.push(exception(
                TrialExceptionKind::AgentTimeout,
                format!(
                    "the agent ran past its {}s deadline",
                    start.agent_timeout_sec
                ),
            ));
            if let Err(error) = driver.cancel() {
                driver.fail(
                    TrialExceptionKind::AgentSettleFailed,
                    format!("cancel at the deadline failed: {}", error.describe()),
                );
                return driver.result;
            }
        }
        Ok(false) => {}
    }
    let Ok(status) = driver.settle() else {
        return driver.result;
    };
    driver.classify_settled(&status);
    let handle = status.trace.clone();
    driver.result.status = Some(status);
    // Finish is settled before the trace is paged; the trace has its own window and never
    // changes the grade.
    driver.result.trace = match handle {
        None => TraceOutcome::Unavailable {
            reason: "the daemon reported no trace".into(),
        },
        Some(handle) => match client.read_trace(pacer, &start.trial, &handle) {
            Ok(jsonl) => TraceOutcome::Complete {
                sha256: trace_sha256(jsonl.as_bytes()),
                jsonl,
            },
            Err(error) => TraceOutcome::Unavailable {
                reason: error.describe(),
            },
        },
    };
    driver.result
}

#[cfg(all(test, unix))]
mod deadline_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::{io::Read, os::unix::net::UnixStream, time::Instant};

    #[test]
    fn delayed_connect_cannot_send_any_method_after_its_cap() {
        for method in [
            METHOD_TASK_START,
            METHOD_TASK_STATUS,
            METHOD_TASK_CANCEL,
            METHOD_TASK_FINISH,
        ] {
            let (sender, mut receiver) = UnixStream::pair().unwrap();
            let budget = Duration::from_millis(20);
            let started = Instant::now();
            let request = format!("{{\"method\":\"{method}\"}}\n");
            let result = exchange_with_connector(request.as_bytes(), budget, |remaining| {
                assert!(remaining <= budget);
                // A connector that uses its whole allowance leaves no time to send anything.
                std::thread::sleep(remaining);
                Ok(sender)
            });
            assert!(matches!(result, Err(WireError::Lost(_))), "{method}");
            assert!(
                started.elapsed() < budget + Duration::from_millis(200),
                "{method}"
            );
            let mut bytes = Vec::new();
            receiver.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes.len(), 0, "{method}: expired connect sent a request");
        }
    }
}
