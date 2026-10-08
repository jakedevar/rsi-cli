//! The one write the gateway can make: answer a waiting manager decision
//! (#990 A2b; design `thoughts/shared/plans/2026-09-28-rsi-remote-first-safe-action.md`).
//!
//! `AnswerGate::serve` is a closed pipeline. Every step before the final
//! daemon call is a denial that dispatches nothing to the answer method:
//!
//! 1. exact `Origin`, bound session cookie and `x-rsi-csrf`;
//! 2. the `allow_answers` switch, read from the policy of THIS request;
//! 3. project scope;
//! 4. per-session rate limit (1 per second, 32 per hour);
//! 5. a typed body that denies unknown fields, answer 1..=2048 bytes without
//!    NUL, idempotency key a canonical UUID;
//! 6. a fresh daemon read of this session's decision targets must contain the
//!    exact `(decision_key, row_version, target_digest)` and fence (a retry of
//!    an identical request this process already forwarded skips the read: the
//!    daemon replays by idempotency key and re-checks everything itself);
//! 7. an `attempt` audit line is durably written (else 503, nothing sent);
//! 8. one `AnswerHarnessManagerDecision` call, then an `outcome` audit line.
//!
//! The audit trail never holds the answer text, only its SHA-256 and length.
//! A transport failure after the request line may have been written is
//! recorded as `unknown`, never as success; the client retries with the same
//! idempotency key and the daemon replays the receipt.

use crate::{
    config::Config,
    ingress::{self, Method, Request},
    reads::{OperatorCall, OperatorDispatch, OperatorError, ReadError},
    session::{self, Binding, SessionDenial, Sessions},
};
use rsi_common::{
    harness_manager_v2::{AnswerHarnessManagerDecisionRequestV2, ManagerMutationReceiptV2},
    remote_decision_targets::{
        DecisionTargetFenceV1, DecisionTargetsManagerV1, RemoteGetDecisionTargetsResponseV1,
        RemoteGetDecisionTargetsV1,
    },
    remote_read::{DecimalI64, Text},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Longest answer text, in bytes.
pub const MAX_ANSWER_BYTES: usize = 2048;
/// Minimum spacing between answer POSTs of one session.
pub const MIN_ANSWER_GAP: Duration = Duration::from_secs(1);
/// Answer POSTs one session may make per [`ANSWER_WINDOW`].
pub const MAX_ANSWERS_PER_WINDOW: usize = 32;
/// The rate window for [`MAX_ANSWERS_PER_WINDOW`].
pub const ANSWER_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Audit file size at which it rotates.
pub const MAX_AUDIT_BYTES: u64 = 1024 * 1024;
/// Rotated audit generations kept next to the active file.
pub const AUDIT_ROTATIONS: u32 = 4;

const AUDIT_FILE: &str = "audit.jsonl";
/// Forwarded requests remembered so a lost-response retry can reach the
/// daemon's idempotency replay after the decision is no longer pending.
const FORWARDED_CAP: usize = 256;
const RETRY_AFTER_BUSY: u64 = 1;

/// The reply the HTTP layer sends. Statuses are limited to the set the
/// gateway's response writer knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerReply {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<u64>,
}

impl AnswerReply {
    const fn bare(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            retry_after: None,
        }
    }

    fn coded(status: u16, code: &str) -> Self {
        Self {
            status,
            body: serde_json::to_vec(&json!({ "error": code })).unwrap_or_default(),
            retry_after: None,
        }
    }

    const fn after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }
}

/// Where audit lines go. An `Err` from [`AuditSink::append`] before an answer
/// is forwarded makes the gateway refuse with 503 and send nothing.
pub trait AuditSink: Send + Sync {
    /// # Errors
    /// Returns the I/O error when the line is not durably written.
    fn append(&self, line: &Value) -> io::Result<()>;
}

/// In-memory sink for tests and embedding; `fail` makes every append error.
#[derive(Default)]
pub struct MemoryAudit {
    lines: Mutex<Vec<Value>>,
    fail: AtomicBool,
}

impl MemoryAudit {
    pub fn lines(&self) -> Vec<Value> {
        lock(&self.lines).clone()
    }

    pub fn set_failing(&self, failing: bool) {
        self.fail.store(failing, Ordering::SeqCst);
    }
}

impl AuditSink for MemoryAudit {
    fn append(&self, line: &Value) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("audit sink failing"));
        }
        lock(&self.lines).push(line.clone());
        Ok(())
    }
}

impl<T: AuditSink + ?Sized> AuditSink for std::sync::Arc<T> {
    fn append(&self, line: &Value) -> io::Result<()> {
        (**self).append(line)
    }
}

/// JSONL audit under `$XDG_STATE_HOME/rsi-remote` (directory 0700, files 0600,
/// 1 MiB per file, [`AUDIT_ROTATIONS`] rotated generations). Existing
/// directories and files that are not owned by this user, are group/world
/// accessible or are symlinks are refused rather than repaired.
pub struct FileAudit {
    dir: PathBuf,
    guard: Mutex<()>,
}

impl FileAudit {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            guard: Mutex::new(()),
        }
    }

    /// `$XDG_STATE_HOME/rsi-remote` (absolute only), else
    /// `$HOME/.local/state/rsi-remote`.
    #[must_use]
    pub fn default_dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| Path::new(&home).join(".local/state"))
                    .filter(|path| path.is_absolute())
            })?;
        Some(base.join("rsi-remote"))
    }

    fn ensure_dir(&self) -> io::Result<()> {
        match fs::symlink_metadata(&self.dir) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.dir)?;
            }
            Err(error) => return Err(error),
        }
        let meta = fs::symlink_metadata(&self.dir)?;
        // SAFETY: geteuid has no preconditions and dereferences no pointers.
        let uid = unsafe { libc::geteuid() };
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe audit directory",
            ));
        }
        Ok(())
    }

    /// `Ok(None)` when absent; otherwise the length of a file that is a
    /// regular file (no symlink), owned by this user and not group/other
    /// accessible. Anything else is refused.
    fn validate_existing(path: &Path) -> io::Result<Option<u64>> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // SAFETY: geteuid has no preconditions and dereferences no pointers.
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe audit file",
            ));
        }
        Ok(Some(meta.len()))
    }

    fn rotated(&self, generation: u32) -> PathBuf {
        self.dir.join(format!("{AUDIT_FILE}.{generation}"))
    }

    fn rotate(&self, active: &Path) -> io::Result<()> {
        for generation in (1..AUDIT_ROTATIONS).rev() {
            let from = self.rotated(generation);
            if fs::symlink_metadata(&from).is_ok() {
                fs::rename(&from, self.rotated(generation + 1))?;
            }
        }
        fs::rename(active, self.rotated(1))
    }
}

impl AuditSink for FileAudit {
    fn append(&self, line: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(line).map_err(io::Error::other)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_AUDIT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "audit line too large",
            ));
        }
        let _held = lock(&self.guard);
        self.ensure_dir()?;
        let active = self.dir.join(AUDIT_FILE);
        // Validate the active file and every existing generation (regular
        // file, our uid, no group/other access) before any rotation, so an
        // unsafe file is never renamed into place or shifted along.
        let active_len = Self::validate_existing(&active)?;
        for generation in 1..=AUDIT_ROTATIONS {
            Self::validate_existing(&self.rotated(generation))?;
        }
        if active_len.is_some_and(|len| len + bytes.len() as u64 > MAX_AUDIT_BYTES) {
            self.rotate(&active)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&active)?;
        let meta = file.metadata()?;
        // SAFETY: geteuid has no preconditions and dereferences no pointers.
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe audit file",
            ));
        }
        file.write_all(&bytes)?;
        file.sync_data()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-session answer rate: one per second and 32 per hour.
#[derive(Default)]
struct RateLimiter {
    by_session: HashMap<String, VecDeque<Instant>>,
}

impl RateLimiter {
    /// Records the attempt, or returns how long until one is allowed.
    fn check(&mut self, session: &str, now: Instant) -> Result<(), Duration> {
        self.by_session.retain(|_, times| {
            while times
                .front()
                .is_some_and(|at| now.saturating_duration_since(*at) >= ANSWER_WINDOW)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = self.by_session.entry(session.to_string()).or_default();
        if let Some(last) = times.back() {
            let since = now.saturating_duration_since(*last);
            if since < MIN_ANSWER_GAP {
                return Err(MIN_ANSWER_GAP.saturating_sub(since));
            }
        }
        if times.len() >= MAX_ANSWERS_PER_WINDOW
            && let Some(oldest) = times.front()
        {
            return Err(ANSWER_WINDOW.saturating_sub(now.saturating_duration_since(*oldest)));
        }
        times.push_back(now);
        Ok(())
    }
}

/// Requests this process forwarded whose outcome was a receipt or unknown,
/// keyed by idempotency key, so an identical retry can reach the daemon's
/// replay. Bounded and short lived; a restart forgets it (the client then sees
/// 409 and polling shows the real state).
#[derive(Default)]
struct Forwarded {
    by_key: HashMap<String, (String, Instant)>,
}

impl Forwarded {
    fn is_repeat(&self, key: &str, payload: &str, now: Instant) -> bool {
        self.by_key.get(key).is_some_and(|(seen, at)| {
            seen == payload && now.saturating_duration_since(*at) < ANSWER_WINDOW
        })
    }

    fn remember(&mut self, key: &str, payload: String, now: Instant) {
        if self.by_key.len() >= FORWARDED_CAP && !self.by_key.contains_key(key) {
            self.by_key
                .retain(|_, (_, at)| now.saturating_duration_since(*at) < ANSWER_WINDOW);
            if self.by_key.len() >= FORWARDED_CAP
                && let Some(oldest) = self
                    .by_key
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(key, _)| key.clone())
            {
                self.by_key.remove(&oldest);
            }
        }
        self.by_key.insert(key.to_string(), (payload, now));
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerBody {
    decision_key: Text<160>,
    expected_row_version: Option<DecimalI64>,
    target_digest: Text<128>,
    fence: Option<DecisionTargetFenceV1>,
    #[serde(skip)]
    pending: bool,
    answer: String,
    idempotency_key: String,
}

fn parse_body(body: &[u8]) -> Option<AnswerBody> {
    let value: Value = serde_json::from_slice(body).ok()?;
    // Derived struct deserialization also accepts positional arrays; the wire
    // contract is objects only.
    if !value.is_object() || !value.get("fence").is_some_and(Value::is_object) {
        return None;
    }
    let parsed: AnswerBody = serde_json::from_value(value).ok()?;
    let answer_ok = !parsed.answer.is_empty()
        && parsed.answer.len() <= MAX_ANSWER_BYTES
        && !parsed.answer.contains('\0');
    (answer_ok
        && ingress::is_canonical_uuid(&parsed.idempotency_key)
        && parsed
            .expected_row_version
            .as_ref()
            .is_some_and(|v| v.get() > 0)
        && parsed
            .fence
            .as_ref()
            .is_some_and(|f| f.policy_version.get() > 0 && f.scope_version.get() > 0))
    .then_some(parsed)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingAnswerBody {
    decision_id: Text<160>,
    expected_target_digest: Text<128>,
    answer: Text<2048>,
    idempotency_key: rsi_common::remote_read::WireUuid,
}

fn parse_pending_body(body: &[u8]) -> Option<AnswerBody> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if !value.is_object() {
        return None;
    }
    let parsed: PendingAnswerBody = serde_json::from_value(value).ok()?;
    if parsed.answer.as_str().contains('\0') {
        return None;
    }
    Some(AnswerBody {
        decision_key: parsed.decision_id,
        target_digest: parsed.expected_target_digest,
        expected_row_version: None,
        fence: None,
        pending: true,
        answer: parsed.answer.as_str().into(),
        idempotency_key: parsed.idempotency_key.as_str().into(),
    })
}

fn targets_contain(response: &RemoteGetDecisionTargetsResponseV1, body: &AnswerBody) -> bool {
    if body.pending {
        return response.pending_items.iter().any(|item| {
            item.decision_id == body.decision_key && item.target_digest == body.target_digest
        });
    }
    response.manager == DecisionTargetsManagerV1::Configured
        && response.fence == body.fence
        && response.items.iter().any(|item| {
            item.decision_key == body.decision_key
                && Some(&item.row_version) == body.expected_row_version.as_ref()
                && item.target_digest == body.target_digest
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// What happened to the forwarded answer, as far as the gateway can know.
enum Outcome {
    Receipt {
        receipt_key: Option<String>,
        deduplicated: Option<bool>,
    },
    Refused {
        changed: bool,
        code: Option<String>,
    },
    /// The call never reached the socket (all permits taken).
    NotSent,
    /// The request line may have been written: delivery is unknown.
    Unknown(ReadError),
}

impl Outcome {
    const fn result(&self) -> &'static str {
        match self {
            Self::Receipt { .. } => "receipt",
            Self::Refused { changed: true, .. } => "refused_changed",
            Self::Refused { .. } => "refused",
            Self::NotSent => "not_sent",
            Self::Unknown(_) => "unknown",
        }
    }
}

fn classify(result: Result<Vec<u8>, OperatorError>) -> (Outcome, Vec<u8>) {
    match result {
        Ok(bytes) => {
            // Success needs a well-formed receipt. Any other object means the
            // daemon's outcome cannot be established: it is unknown, so the
            // same-key retry stays possible.
            let Ok(receipt) = serde_json::from_slice::<ManagerMutationReceiptV2>(&bytes) else {
                return (Outcome::Unknown(ReadError::Malformed), Vec::new());
            };
            let outcome = Outcome::Receipt {
                receipt_key: Some(receipt.key.chars().take(128).collect()),
                deduplicated: Some(receipt.deduplicated),
            };
            (outcome, bytes)
        }
        // Matched here, never through `OperatorError::transport()`, which
        // folds a refusal into a transport error.
        Err(OperatorError::Refused(message)) => {
            let code = (!message.is_empty()
                && message.len() <= 64
                && message
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
            .then(|| message.clone());
            (
                Outcome::Refused {
                    changed: message.contains("changed"),
                    code,
                },
                Vec::new(),
            )
        }
        Err(OperatorError::Transport(ReadError::Busy)) => (Outcome::NotSent, Vec::new()),
        Err(OperatorError::Transport(error)) => (Outcome::Unknown(error), Vec::new()),
    }
}

fn classify_pending(result: Result<Vec<u8>, OperatorError>) -> (Outcome, Vec<u8>) {
    match result {
        Ok(bytes) => match serde_json::from_slice::<
            rsi_common::remote_pending_decisions::RemoteAnswerReceiptV1,
        >(&bytes)
        {
            Ok(receipt) => (
                Outcome::Receipt {
                    receipt_key: Some(receipt.receipt_key.as_str().into()),
                    deduplicated: None,
                },
                bytes,
            ),
            Err(_) => (Outcome::Unknown(ReadError::Malformed), Vec::new()),
        },
        Err(error) => classify(Err(error)),
    }
}

/// Everything about one request that is not gateway-wide state.
pub struct AnswerRequest<'a> {
    pub req: &'a Request,
    pub body: &'a [u8],
    pub origin_ok: bool,
    pub binding: &'a Binding,
    pub sessions: &'a Sessions,
    pub policy: &'a Config,
    pub now: Instant,
}

/// Gateway-wide answer state: the audit sink, rate limiter and the bounded
/// memory of forwarded requests.
pub struct AnswerGate {
    audit: Box<dyn AuditSink>,
    gateway_epoch: uuid::Uuid,
    limiter: Mutex<RateLimiter>,
    forwarded: Mutex<Forwarded>,
}

impl AnswerGate {
    #[must_use]
    pub fn new(audit: Box<dyn AuditSink>) -> Self {
        Self {
            audit,
            gateway_epoch: uuid::Uuid::new_v4(),
            limiter: Mutex::new(RateLimiter::default()),
            forwarded: Mutex::new(Forwarded::default()),
        }
    }

    /// Run the whole answer pipeline for one answer POST.
    pub async fn serve<D: OperatorDispatch>(
        &self,
        input: &AnswerRequest<'_>,
        dispatch: &D,
    ) -> AnswerReply {
        let AnswerRequest {
            req,
            body,
            origin_ok,
            binding,
            sessions,
            policy,
            now,
        } = *input;
        let Some((project, session)) = ingress::answer_route(&req.path) else {
            return AnswerReply::bare(404);
        };
        let (project, session) = (project.as_str(), session.as_str());
        if req.method != Method::Post {
            return AnswerReply::bare(404);
        }
        // An answer needs the Origin header itself, not the Fetch Metadata
        // fallback the read routes accept.
        let exact_origin =
            req.origin.as_deref() == Some(format!("https://{}", policy.canonical_host).as_str());
        let cookie = session::cookie(req.cookie.as_deref(), session::SESSION_COOKIE);
        if let Err(denial) = sessions.check_csrf(
            cookie,
            req.csrf.as_deref(),
            origin_ok && exact_origin,
            binding,
            now,
        ) {
            return AnswerReply::bare(match denial {
                SessionDenial::BadOrigin | SessionDenial::BadCsrf => 403,
                _ => 401,
            });
        }
        let Some(cookie) = cookie else {
            return AnswerReply::bare(401);
        };
        if !policy.allow_answers || !policy.project_ids.iter().any(|allowed| allowed == project) {
            return AnswerReply::bare(403);
        }
        let limited = lock(&self.limiter).check(cookie, now);
        if let Err(wait) = limited {
            return AnswerReply::coded(429, "rate_limited")
                .after(wait.as_secs() + u64::from(wait.subsec_nanos() > 0));
        }
        let pending = req.path.ends_with("/decisions/answer-pending");
        let Some(parsed) = (if pending {
            parse_pending_body(body)
        } else {
            parse_body(body)
        }) else {
            return AnswerReply::coded(400, "invalid_answer");
        };

        let call = if pending {
            match serde_json::from_value(json!({
                "project_id": project, "session_id": session,
                "decision_id": parsed.decision_key.as_str(), "expected_target_digest": parsed.target_digest.as_str(),
                "answer": parsed.answer, "idempotency_key": parsed.idempotency_key,
                "origin": {"kind":"remote", "client_node":binding.client_node, "gateway_epoch":self.gateway_epoch.to_string()}
            })) {
                Ok(request) => OperatorCall::AnswerPending(request),
                Err(_) => return AnswerReply::coded(400, "invalid_answer"),
            }
        } else {
            let fence = parsed.fence.as_ref().expect("validated manager fence");
            match serde_json::from_value::<AnswerHarnessManagerDecisionRequestV2>(json!({
                "project_id": project,
                "fence": {"scope_version":fence.scope_version.get(), "policy_version":fence.policy_version.get()},
                "decision_key":parsed.decision_key.as_str(), "expected_row_version":parsed.expected_row_version.as_ref().expect("validated version").get(),
                "target_digest":parsed.target_digest.as_str(), "answer":parsed.answer, "idempotency_key":parsed.idempotency_key,
            })) {
                Ok(request) => OperatorCall::Answer(request),
                Err(_) => return AnswerReply::coded(400, "invalid_answer"),
            }
        };
        let payload =
            sha256_hex(&serde_json::to_vec(&call.wire().unwrap_or_default()).unwrap_or_default());
        let replay = lock(&self.forwarded).is_repeat(&parsed.idempotency_key, &payload, now);

        // Pending-answer ingress replays its durable receipt before reading
        // the target. A fresh gateway must reach that fence even when A1 no
        // longer lists an answered occurrence. The daemon validates new keys.
        if !pending
            && !replay
            && let Some(reply) = Self::check_targets(project, session, &parsed, dispatch).await
        {
            return reply;
        }

        let session_digest = &sha256_hex(cookie.as_bytes())[..16];
        let common = |phase: &str| {
            json!({
                "ts": now_rfc3339(),
                "phase": phase,
                "node": binding.client_node,
                "session_digest": session_digest,
                "project_id": project,
                "session_id": session,
                "decision_key": parsed.decision_key.as_str(),
                "row_version": parsed.expected_row_version.as_ref().map(DecimalI64::get),
                "owner": binding.owner_user_id,
                "idempotency_key": parsed.idempotency_key,
                "answer_sha256": sha256_hex(parsed.answer.as_bytes()),
                "answer_len": parsed.answer.len(),
                "replay": replay,
            })
        };
        if self.audit.append(&common("attempt")).is_err() {
            eprintln!("rsi-remote: answer refused: audit unavailable");
            return AnswerReply::coded(503, "audit_unavailable").after(RETRY_AFTER_BUSY);
        }

        let result = dispatch.call(&call).await;
        let (outcome, receipt) = if pending {
            classify_pending(result)
        } else {
            classify(result)
        };
        let mut line = common("outcome");
        line["result"] = json!(outcome.result());
        let reply = match &outcome {
            Outcome::Receipt {
                receipt_key,
                deduplicated,
            } => {
                lock(&self.forwarded).remember(&parsed.idempotency_key, payload, now);
                line["receipt_key"] = json!(receipt_key);
                line["deduplicated"] = json!(deduplicated);
                AnswerReply {
                    status: 200,
                    body: receipt,
                    retry_after: None,
                }
            }
            Outcome::Refused { changed, code } => {
                line["daemon_code"] = json!(code);
                if *changed {
                    AnswerReply::coded(409, "decision_changed")
                } else {
                    AnswerReply::bare(403)
                }
            }
            Outcome::NotSent => AnswerReply::coded(503, "busy").after(RETRY_AFTER_BUSY),
            Outcome::Unknown(error) => {
                lock(&self.forwarded).remember(&parsed.idempotency_key, payload, now);
                line["transport"] = json!(error.code());
                AnswerReply::coded(
                    if *error == ReadError::Timeout {
                        504
                    } else {
                        502
                    },
                    "outcome_unknown",
                )
            }
        };
        line["status"] = json!(reply.status);
        if self.audit.append(&line).is_err() {
            // The answer is already decided; nothing can be undone. Say so on
            // the host log instead of changing the reply.
            eprintln!("rsi-remote: answer outcome audit line not written");
        }
        reply
    }

    /// `None` when the fresh daemon targets contain the exact decision.
    async fn check_targets<D: OperatorDispatch>(
        project: &str,
        session: &str,
        body: &AnswerBody,
        dispatch: &D,
    ) -> Option<AnswerReply> {
        let targets: RemoteGetDecisionTargetsV1 = serde_json::from_value(json!({
            "project_id": project,
            "session_id": session,
        }))
        .ok()?;
        match dispatch.call(&OperatorCall::DecisionTargets(targets)).await {
            Ok(bytes) => match serde_json::from_slice::<RemoteGetDecisionTargetsResponseV1>(&bytes)
            {
                Ok(response) if targets_contain(&response, body) => None,
                Ok(_) => Some(AnswerReply::coded(409, "decision_changed")),
                Err(_) => Some(AnswerReply::bare(502)),
            },
            Err(OperatorError::Transport(ReadError::Busy)) => {
                Some(AnswerReply::coded(503, "busy").after(RETRY_AFTER_BUSY))
            }
            Err(OperatorError::Transport(ReadError::Timeout)) => Some(AnswerReply::bare(504)),
            Err(_) => Some(AnswerReply::bare(502)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_one_per_second_and_32_per_hour() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();
        assert!(limiter.check("a", start).is_ok());
        let wait = limiter
            .check("a", start + Duration::from_millis(400))
            .unwrap_err();
        assert_eq!(wait, Duration::from_millis(600));
        assert!(limiter.check("b", start).is_ok(), "per session");
        for i in 1..32u64 {
            assert!(
                limiter
                    .check("a", start + Duration::from_secs(i * 2))
                    .is_ok()
            );
        }
        let wait = limiter
            .check("a", start + Duration::from_secs(100))
            .unwrap_err();
        assert_eq!(wait, ANSWER_WINDOW - Duration::from_secs(100));
        assert!(
            limiter
                .check("a", start + ANSWER_WINDOW + Duration::from_secs(1))
                .is_ok(),
            "the oldest attempt ages out"
        );
    }

    #[test]
    fn forwarded_memory_is_exact_bounded_and_expires() {
        let mut forwarded = Forwarded::default();
        let start = Instant::now();
        forwarded.remember("k", "p".into(), start);
        assert!(forwarded.is_repeat("k", "p", start));
        assert!(!forwarded.is_repeat("k", "other", start));
        assert!(!forwarded.is_repeat("k", "p", start + ANSWER_WINDOW));
        for i in 0..(FORWARDED_CAP + 10) {
            forwarded.remember(&format!("key{i}"), "p".into(), start);
        }
        assert!(forwarded.by_key.len() <= FORWARDED_CAP);
    }

    #[test]
    fn classify_matches_refusals_and_marks_transport_failures_unknown() {
        let (outcome, _) = classify(Err(OperatorError::Refused(
            "manager_v2_decision_changed".into(),
        )));
        assert!(matches!(
            outcome,
            Outcome::Refused {
                changed: true,
                code: Some(_)
            }
        ));
        let (outcome, _) = classify(Err(OperatorError::Refused("other".into())));
        assert_eq!(outcome.result(), "refused");
        let (outcome, _) = classify(Err(OperatorError::Refused(
            "Contains The Answer Text".into(),
        )));
        assert!(matches!(outcome, Outcome::Refused { code: None, .. }));
        assert_eq!(
            classify(Err(OperatorError::Transport(ReadError::Busy)))
                .0
                .result(),
            "not_sent"
        );
        for error in [
            ReadError::Timeout,
            ReadError::Connect,
            ReadError::Oversize,
            ReadError::Malformed,
        ] {
            assert_eq!(
                classify(Err(OperatorError::Transport(error))).0.result(),
                "unknown"
            );
        }
    }

    #[test]
    fn body_grammar() {
        let good = |answer: &str, idem: &str| {
            json!({
                "decision_key": "question:550e8400-e29b-41d4-a716-446655440001",
                "expected_row_version": "3",
                "target_digest": "abc",
                "fence": {"policy_version": "2", "scope_version": "1"},
                "answer": answer,
                "idempotency_key": idem,
            })
            .to_string()
        };
        let idem = "550e8400-e29b-41d4-a716-446655440009";
        assert!(parse_body(good("yes", idem).as_bytes()).is_some());
        assert!(parse_body(good(&"x".repeat(MAX_ANSWER_BYTES), idem).as_bytes()).is_some());
        assert!(parse_body(good(&"x".repeat(MAX_ANSWER_BYTES + 1), idem).as_bytes()).is_none());
        assert!(parse_body(good("", idem).as_bytes()).is_none());
        assert!(parse_body(good("a\u{0}b", idem).as_bytes()).is_none());
        assert!(parse_body(good("yes", "not-a-uuid").as_bytes()).is_none());
        assert!(
            parse_body(good("yes", &idem.to_uppercase()).as_bytes()).is_none(),
            "canonical lowercase only"
        );
        let extra = good("yes", idem).replace("\"answer\"", "\"extra\":1,\"answer\"");
        assert!(parse_body(extra.as_bytes()).is_none());
        assert!(parse_body(b"[]").is_none());
        let zero = good("yes", idem).replace("\"3\"", "\"0\"");
        assert!(parse_body(zero.as_bytes()).is_none());
        let numeric = good("yes", idem).replace("\"3\"", "3");
        assert!(
            parse_body(numeric.as_bytes()).is_none(),
            "versions are decimal strings"
        );
        let positional_fence = good("yes", idem).replace(
            "{\"policy_version\":\"2\",\"scope_version\":\"1\"}",
            "[\"2\",\"1\"]",
        );
        assert!(parse_body(positional_fence.as_bytes()).is_none());
    }

    #[test]
    fn file_audit_is_private_rotates_and_refuses_unsafe_paths() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state/rsi-remote");
        let audit = FileAudit::new(&dir);
        audit.append(&json!({"n": 1})).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(dir.join(AUDIT_FILE)).unwrap().mode() & 0o777,
            0o600
        );

        let filler = json!({"pad": "x".repeat(200_000)});
        for _ in 0..30 {
            audit.append(&filler).unwrap();
        }
        let names: Vec<_> = (1..=AUDIT_ROTATIONS)
            .map(|n| dir.join(format!("{AUDIT_FILE}.{n}")))
            .collect();
        for path in &names {
            let meta = fs::metadata(path).unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
            assert!(meta.len() <= MAX_AUDIT_BYTES);
        }
        assert!(
            !dir.join(format!("{AUDIT_FILE}.{}", AUDIT_ROTATIONS + 1))
                .exists(),
            "oldest generation is dropped"
        );
        assert!(fs::metadata(dir.join(AUDIT_FILE)).unwrap().len() <= MAX_AUDIT_BYTES);

        let wide = root.path().join("wide");
        fs::create_dir(&wide).unwrap();
        fs::set_permissions(&wide, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FileAudit::new(&wide).append(&json!({})).is_err());

        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(FileAudit::new(&link).append(&json!({})).is_err());

        let loose = root.path().join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(loose.join(AUDIT_FILE), b"").unwrap();
        fs::set_permissions(loose.join(AUDIT_FILE), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(FileAudit::new(&loose).append(&json!({})).is_err());
    }

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const SESSION: &str = "550e8400-e29b-41d4-a716-446655440001";
    const IDEM: &str = "550e8400-e29b-41d4-a716-446655440009";
    const HOST: &str = "host.example.ts.net";
    const ANSWER_TEXT: &str = "ship it, tell nobody secret-token-text";

    fn policy(allow_answers: bool) -> Config {
        Config {
            enabled: true,
            canonical_host: HOST.to_string(),
            owner_user_id: 501,
            allowed_node_ids: vec!["nHOME".to_string()],
            project_ids: vec![PROJECT.to_string()],
            allow_answers,
        }
    }

    fn binding() -> Binding {
        Binding {
            owner_user_id: 501,
            client_node: "nCLIENT".to_string(),
            server_node: "nHOME".to_string(),
        }
    }

    fn targets_json(version: &str, digest: &str) -> Vec<u8> {
        json!({
            "manager": "configured",
            "fence": {"policy_version": "2", "scope_version": "1"},
            "items": [{
                "decision_key": format!("question:{SESSION}"),
                "row_version": version,
                "target_digest": digest,
                "kind": "question",
                "title": "Ship?",
                "detail": "",
                "options": [],
            }],
            "truncated": false,
        })
        .to_string()
        .into_bytes()
    }

    fn body_json(answer: &str) -> Vec<u8> {
        json!({
            "decision_key": format!("question:{SESSION}"),
            "expected_row_version": "3",
            "target_digest": "digest-a",
            "fence": {"policy_version": "2", "scope_version": "1"},
            "answer": answer,
            "idempotency_key": IDEM,
        })
        .to_string()
        .into_bytes()
    }

    struct Fake {
        targets: Mutex<Result<Vec<u8>, OperatorError>>,
        answer: Mutex<Result<Vec<u8>, OperatorError>>,
        target_calls: Mutex<Vec<Value>>,
        answer_calls: Mutex<Vec<Value>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                targets: Mutex::new(Ok(targets_json("3", "digest-a"))),
                answer: Mutex::new(Ok(
                    br#"{"event_sequence":9,"key":"receipt-key","row_version":4,"deduplicated":false}"#
                        .to_vec(),
                )),
                target_calls: Mutex::new(Vec::new()),
                answer_calls: Mutex::new(Vec::new()),
            }
        }

        fn targets(&self) -> usize {
            lock(&self.target_calls).len()
        }

        fn answers(&self) -> usize {
            lock(&self.answer_calls).len()
        }
    }

    impl OperatorDispatch for Fake {
        async fn call(&self, call: &OperatorCall) -> Result<Vec<u8>, OperatorError> {
            match call {
                OperatorCall::DecisionTargets(request) => {
                    lock(&self.target_calls).push(serde_json::to_value(request).unwrap());
                    lock(&self.targets).clone()
                }
                OperatorCall::AnswerPending(request) => {
                    lock(&self.answer_calls).push(serde_json::to_value(request).unwrap());
                    lock(&self.answer).clone()
                }
                OperatorCall::Answer(request) => {
                    lock(&self.answer_calls).push(serde_json::to_value(request).unwrap());
                    lock(&self.answer).clone()
                }
            }
        }
    }

    struct Rig {
        sessions: Sessions,
        gate: AnswerGate,
        audit: std::sync::Arc<MemoryAudit>,
        fake: Fake,
        cookie: String,
        csrf: String,
        now: Instant,
    }

    impl Rig {
        fn new() -> Self {
            let now = Instant::now();
            let sessions = Sessions::new();
            let boot = sessions.bootstrap(&binding(), now).unwrap();
            let established = sessions
                .establish(Some(&boot.cookie), &boot.secret, true, &binding(), now)
                .unwrap();
            let audit = std::sync::Arc::new(MemoryAudit::default());
            Self {
                sessions,
                gate: AnswerGate::new(Box::new(audit.clone())),
                audit,
                fake: Fake::new(),
                cookie: established.cookie,
                csrf: established.secret,
                now,
            }
        }

        fn request(&self) -> Request {
            Request {
                method: Method::Post,
                path: format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}/decisions/answer"),
                query: ingress::Query::default(),
                source: "100.101.102.103".parse().unwrap(),
                content_length: 0,
                origin: Some(format!("https://{HOST}")),
                sec_fetch_site: None,
                cookie: Some(format!("{}={}", session::SESSION_COOKIE, self.cookie)),
                csrf: Some(self.csrf.clone()),
            }
        }

        async fn post(
            &self,
            req: &Request,
            body: &[u8],
            policy: &Config,
            at: Duration,
        ) -> AnswerReply {
            let origin_ok =
                session::origin_ok(req.origin.as_deref(), req.sec_fetch_site.as_deref(), HOST);
            let input = AnswerRequest {
                req,
                body,
                origin_ok,
                binding: &binding(),
                sessions: &self.sessions,
                policy,
                now: self.now + at,
            };
            self.gate.serve(&input, &self.fake).await
        }

        async fn answer(&self) -> AnswerReply {
            self.post(
                &self.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::ZERO,
            )
            .await
        }
    }

    fn assert_zero_dispatch(rig: &Rig, reply: &AnswerReply, status: u16) {
        assert_eq!(reply.status, status);
        assert_eq!(rig.fake.targets(), 0, "no daemon read");
        assert_eq!(rig.fake.answers(), 0, "no daemon answer");
        assert!(rig.audit.lines().is_empty(), "no audit line");
    }

    #[tokio::test]
    async fn every_gateway_denial_dispatches_nothing() {
        let rig = Rig::new();
        let ok = || rig.request();
        let body = body_json(ANSWER_TEXT);

        let mut no_cookie = ok();
        no_cookie.cookie = None;
        let reply = rig
            .post(&no_cookie, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 401);

        let mut wrong_cookie = ok();
        wrong_cookie.cookie = Some(format!("{}={}", session::SESSION_COOKIE, "0".repeat(64)));
        let reply = rig
            .post(&wrong_cookie, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 401);

        let mut no_csrf = ok();
        no_csrf.csrf = None;
        let reply = rig
            .post(&no_csrf, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 403);

        let mut bad_csrf = ok();
        bad_csrf.csrf = Some("f".repeat(64));
        let reply = rig
            .post(&bad_csrf, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 403);

        let mut foreign_origin = ok();
        foreign_origin.origin = Some("https://evil.example".to_string());
        let reply = rig
            .post(&foreign_origin, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 403);

        let mut fetch_metadata_only = ok();
        fetch_metadata_only.origin = None;
        fetch_metadata_only.sec_fetch_site = Some("same-origin".to_string());
        let reply = rig
            .post(&fetch_metadata_only, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 403);

        let reply = rig.post(&ok(), &body, &policy(false), Duration::ZERO).await;
        assert_zero_dispatch(&rig, &reply, 403);

        let mut other_project = policy(true);
        other_project.project_ids = vec!["550e8400-e29b-41d4-a716-4466554400ff".to_string()];
        let reply = rig.post(&ok(), &body, &other_project, Duration::ZERO).await;
        assert_zero_dispatch(&rig, &reply, 403);

        let mut get = ok();
        get.method = Method::Get;
        let reply = rig.post(&get, &body, &policy(true), Duration::ZERO).await;
        assert_zero_dispatch(&rig, &reply, 404);

        let mut off_route = ok();
        off_route.path = format!("/api/v1/projects/{PROJECT}/sessions/{SESSION}/decisions");
        let reply = rig
            .post(&off_route, &body, &policy(true), Duration::ZERO)
            .await;
        assert_zero_dispatch(&rig, &reply, 404);
    }

    #[tokio::test]
    async fn every_malformed_body_dispatches_nothing() {
        let rig = Rig::new();
        let bodies: Vec<Vec<u8>> = vec![
            b"not json".to_vec(),
            b"[]".to_vec(),
            body_json(""),
            body_json(&"x".repeat(MAX_ANSWER_BYTES + 1)),
            body_json("nul\u{0}byte"),
            String::from_utf8(body_json("a"))
                .unwrap()
                .replace(IDEM, "not-a-uuid")
                .into_bytes(),
            String::from_utf8(body_json("a"))
                .unwrap()
                .replace("\"answer\"", "\"unknown\":1,\"answer\"")
                .into_bytes(),
        ];
        for (i, body) in bodies.iter().enumerate() {
            let reply = rig
                .post(
                    &rig.request(),
                    body,
                    &policy(true),
                    Duration::from_secs(2 * i as u64),
                )
                .await;
            assert_zero_dispatch(&rig, &reply, 400);
        }
    }

    #[tokio::test]
    async fn rate_limits_dispatch_nothing_and_say_when_to_retry() {
        let rig = Rig::new();
        assert_eq!(rig.answer().await.status, 200);
        assert_eq!(rig.fake.answers(), 1);
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_millis(300),
            )
            .await;
        assert_eq!(reply.status, 429);
        assert_eq!(reply.retry_after, Some(1));
        assert_eq!(rig.fake.answers(), 1, "limited request was not forwarded");
    }

    #[tokio::test]
    async fn stale_targets_are_409_before_any_answer_or_audit() {
        for stale in [
            targets_json("4", "digest-a"),
            targets_json("3", "digest-b"),
            br#"{"manager":"not_configured","fence":null,"items":[],"truncated":false}"#.to_vec(),
            json!({
                "manager": "configured",
                "fence": {"policy_version": "9", "scope_version": "1"},
                "items": [],
                "truncated": false,
            })
            .to_string()
            .into_bytes(),
        ] {
            let rig = Rig::new();
            *lock(&rig.fake.targets) = Ok(stale);
            let reply = rig.answer().await;
            assert_eq!(reply.status, 409);
            assert_eq!(rig.fake.targets(), 1);
            assert_eq!(rig.fake.answers(), 0);
            assert!(rig.audit.lines().is_empty());
        }
        let rig = Rig::new();
        *lock(&rig.fake.targets) = Ok(b"not targets".to_vec());
        assert_eq!(rig.answer().await.status, 502);
        *lock(&rig.fake.targets) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!((reply.status, reply.retry_after), (503, Some(1)));
        assert_eq!(rig.fake.answers(), 0);
    }

    #[tokio::test]
    async fn forwards_exactly_the_typed_request_and_audits_without_the_answer() {
        let rig = Rig::new();
        let reply = rig.answer().await;
        assert_eq!(reply.status, 200);
        let receipt: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(receipt["key"], "receipt-key");

        assert_eq!(
            lock(&rig.fake.target_calls)[0],
            json!({"project_id": PROJECT, "session_id": SESSION})
        );
        assert_eq!(
            lock(&rig.fake.answer_calls)[0],
            json!({
                "project_id": PROJECT,
                "fence": {"scope_version": 1, "policy_version": 2},
                "decision_key": format!("question:{SESSION}"),
                "expected_row_version": 3,
                "target_digest": "digest-a",
                "answer": ANSWER_TEXT,
                "idempotency_key": IDEM,
            })
        );

        let lines = rig.audit.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["phase"], "attempt");
        assert_eq!(lines[1]["phase"], "outcome");
        assert_eq!(lines[1]["result"], "receipt");
        assert_eq!(lines[1]["receipt_key"], "receipt-key");
        assert_eq!(lines[1]["status"], 200);
        for line in &lines {
            assert_eq!(line["node"], "nCLIENT");
            assert_eq!(line["project_id"], PROJECT);
            assert_eq!(line["idempotency_key"], IDEM);
            assert_eq!(line["answer_sha256"], sha256_hex(ANSWER_TEXT.as_bytes()));
            assert_eq!(line["answer_len"], ANSWER_TEXT.len());
        }
        let everything = serde_json::to_string(&lines).unwrap();
        assert!(!everything.contains("secret-token-text"), "never the text");
        assert!(
            !everything.contains(&rig.cookie),
            "never the session secret"
        );
        assert!(!everything.contains(&rig.csrf), "never the csrf token");
    }

    #[tokio::test]
    async fn refusals_map_to_409_for_changed_and_403_otherwise() {
        for (message, status) in [
            ("manager_v2_decision_changed", 409),
            ("manager_v2_decision_scope_changed", 409),
            ("manager_v2_decision_target_changed", 409),
            ("manager_v2_scope_denied", 403),
            ("manager_not_configured", 403),
        ] {
            let rig = Rig::new();
            *lock(&rig.fake.answer) = Err(OperatorError::Refused(message.to_string()));
            let reply = rig.answer().await;
            assert_eq!(reply.status, status, "{message}");
            assert_eq!(rig.fake.answers(), 1);
            let lines = rig.audit.lines();
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[1]["daemon_code"], message);
            assert_eq!(
                lines[1]["result"],
                if status == 409 {
                    "refused_changed"
                } else {
                    "refused"
                }
            );
        }
    }

    #[tokio::test]
    async fn busy_is_a_retryable_503_and_never_recorded_as_sent() {
        let rig = Rig::new();
        *lock(&rig.fake.answer) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig.answer().await;
        assert_eq!((reply.status, reply.retry_after), (503, Some(1)));
        assert_eq!(rig.audit.lines()[1]["result"], "not_sent");
    }

    #[tokio::test]
    async fn a_timeout_after_the_write_is_recorded_unknown_and_retries_replay() {
        let rig = Rig::new();
        *lock(&rig.fake.answer) = Err(OperatorError::Transport(ReadError::Timeout));
        let reply = rig.answer().await;
        assert_eq!(reply.status, 504);
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap(),
            json!({"error": "outcome_unknown"})
        );
        let lines = rig.audit.lines();
        assert_eq!(lines[1]["result"], "unknown");
        assert_eq!(lines[1]["transport"], "timeout");
        assert_ne!(lines[1]["result"], "receipt");

        // The decision may have been applied, so it is no longer pending. The
        // client retries with the same key; the identical request reaches the
        // daemon's replay without a fresh target match.
        *lock(&rig.fake.targets) =
            Ok(br#"{"manager":"configured","fence":null,"items":[],"truncated":false}"#.to_vec());
        *lock(&rig.fake.answer) = Ok(
            br#"{"event_sequence":9,"key":"receipt-key","row_version":4,"deduplicated":true}"#
                .to_vec(),
        );
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(2),
            )
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(
            rig.fake.targets(),
            1,
            "no second target read for an identical retry"
        );
        assert_eq!(rig.fake.answers(), 2);
        let lines = rig.audit.lines();
        assert_eq!(lines[2]["replay"], true);
        assert_eq!(lines[3]["deduplicated"], true);

        // A different answer under the same key is a new request: it needs
        // current targets, which no longer match.
        let reply = rig
            .post(
                &rig.request(),
                &body_json("something else"),
                &policy(true),
                Duration::from_secs(4),
            )
            .await;
        assert_eq!(reply.status, 409);
        assert_eq!(rig.fake.answers(), 2);
    }

    #[tokio::test]
    async fn an_unwritable_audit_refuses_with_503_and_sends_nothing() {
        let rig = Rig::new();
        rig.audit.set_failing(true);
        let reply = rig.answer().await;
        assert_eq!(reply.status, 503);
        assert_eq!(rig.fake.answers(), 0);
        assert!(rig.audit.lines().is_empty());
    }

    #[test]
    fn classify_treats_an_object_that_is_not_a_receipt_as_unknown() {
        for bytes in [
            &br#"{}"#[..],
            br#"{"key":"receipt-key"}"#,
            br#"{"event_sequence":"9","key":"receipt-key","row_version":4,"deduplicated":false}"#,
            br#"{"event_sequence":9,"key":"receipt-key","row_version":4,"deduplicated":"yes"}"#,
            b"not json",
        ] {
            let (outcome, body) = classify(Ok(bytes.to_vec()));
            assert!(matches!(outcome, Outcome::Unknown(ReadError::Malformed)));
            assert!(body.is_empty());
        }
        let (outcome, _) = classify(Ok(
            br#"{"event_sequence":9,"key":"receipt-key","row_version":4,"deduplicated":false}"#
                .to_vec(),
        ));
        assert_eq!(outcome.result(), "receipt");
    }

    #[tokio::test]
    async fn an_invalid_receipt_is_502_outcome_unknown_and_keeps_the_same_key_retry() {
        let rig = Rig::new();
        for (n, invalid) in [
            &br#"{}"#[..],
            br#"{"event_sequence":"9","key":"receipt-key","row_version":4,"deduplicated":false}"#,
        ]
        .into_iter()
        .enumerate()
        {
            *lock(&rig.fake.answer) = Ok(invalid.to_vec());
            let reply = rig
                .post(
                    &rig.request(),
                    &body_json(ANSWER_TEXT),
                    &policy(true),
                    Duration::from_secs(2 * (n as u64 + 1)),
                )
                .await;
            assert_eq!(reply.status, 502);
            let lines = rig.audit.lines();
            let last = lines.last().unwrap();
            assert_eq!(last["result"], "unknown");
            assert_eq!(last["transport"], "malformed");
            assert_eq!(rig.fake.answers(), n + 1);
        }
        // The decision may have been applied: the identical retry reaches the
        // daemon's replay without a second target read.
        *lock(&rig.fake.targets) =
            Ok(br#"{"manager":"configured","fence":null,"items":[],"truncated":false}"#.to_vec());
        *lock(&rig.fake.answer) = Ok(
            br#"{"event_sequence":9,"key":"receipt-key","row_version":4,"deduplicated":true}"#
                .to_vec(),
        );
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(6),
            )
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(rig.fake.answers(), 3);
    }

    #[tokio::test]
    async fn an_unsafe_audit_file_or_generation_refuses_with_503_before_any_rotation() {
        use std::os::unix::fs::PermissionsExt;
        let mut rig = Rig::new();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        // An oversized, permissive active file must be refused in place, not
        // renamed into a rotated generation.
        let active = dir.join(AUDIT_FILE);
        fs::write(&active, vec![b'x'; MAX_AUDIT_BYTES as usize]).unwrap();
        fs::set_permissions(&active, fs::Permissions::from_mode(0o644)).unwrap();
        rig.gate = AnswerGate::new(Box::new(FileAudit::new(&dir)));
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(2),
            )
            .await;
        assert_eq!(reply.status, 503);
        assert_eq!(rig.fake.answers(), 0);
        assert_eq!(fs::metadata(&active).unwrap().len(), MAX_AUDIT_BYTES);
        assert!(!dir.join(format!("{AUDIT_FILE}.1")).exists());

        // A safe but full active file with an unsafe rotated generation must
        // not shift any generation.
        fs::set_permissions(&active, fs::Permissions::from_mode(0o600)).unwrap();
        let unsafe_generation = dir.join(format!("{AUDIT_FILE}.2"));
        fs::write(&unsafe_generation, b"old").unwrap();
        fs::set_permissions(&unsafe_generation, fs::Permissions::from_mode(0o644)).unwrap();
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(4),
            )
            .await;
        assert_eq!(reply.status, 503);
        assert_eq!(rig.fake.answers(), 0);
        assert_eq!(fs::metadata(&active).unwrap().len(), MAX_AUDIT_BYTES);
        assert!(!dir.join(format!("{AUDIT_FILE}.1")).exists());
        assert_eq!(fs::read(&unsafe_generation).unwrap(), b"old");

        // A symlinked generation is refused the same way.
        fs::remove_file(&unsafe_generation).unwrap();
        let target = root.path().join("elsewhere");
        fs::write(&target, b"other").unwrap();
        std::os::unix::fs::symlink(&target, &unsafe_generation).unwrap();
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(6),
            )
            .await;
        assert_eq!(reply.status, 503);
        assert_eq!(rig.fake.answers(), 0);
        assert!(!dir.join(format!("{AUDIT_FILE}.1")).exists());
        assert_eq!(fs::read(&target).unwrap(), b"other");

        // Once only validated files remain, the same full file rotates.
        fs::remove_file(&unsafe_generation).unwrap();
        fs::write(&unsafe_generation, b"old").unwrap();
        fs::set_permissions(&unsafe_generation, fs::Permissions::from_mode(0o600)).unwrap();
        let reply = rig
            .post(
                &rig.request(),
                &body_json(ANSWER_TEXT),
                &policy(true),
                Duration::from_secs(8),
            )
            .await;
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        assert_eq!(rig.fake.answers(), 1);
        assert!(dir.join(format!("{AUDIT_FILE}.1")).exists());
        assert!(dir.join(format!("{AUDIT_FILE}.3")).exists());
    }

    #[test]
    fn default_dir_prefers_absolute_xdg_state_home() {
        // Environment is process-global; only the pure join is asserted here.
        let dir = FileAudit::default_dir();
        if let Some(dir) = dir {
            assert!(dir.ends_with("rsi-remote"));
            assert!(dir.is_absolute());
        }
    }
    #[tokio::test]
    async fn pending_answer_retry_after_gateway_restart_reaches_receipt_without_a1_target() {
        let mut rig = Rig::new();
        let mut req = rig.request();
        req.path = req.path.replace("/answer", "/answer-pending");
        let body = json!({"decision_id":format!("pending-question:{SESSION}"),
            "expected_target_digest":"digest-pending", "answer":"Proceed", "idempotency_key":IDEM})
        .to_string()
        .into_bytes();
        *lock(&rig.fake.answer) = Err(OperatorError::Transport(ReadError::Timeout));
        assert_eq!(
            rig.post(&req, &body, &policy(true), Duration::ZERO)
                .await
                .status,
            504
        );
        let original_epoch = lock(&rig.fake.answer_calls)[0]["origin"]["gateway_epoch"].clone();
        // Restart loses forwarded memory and assigns a new epoch. A1 has no
        // pending target because the original answer was already delivered.
        rig.gate = AnswerGate::new(Box::new(rig.audit.clone()));
        let receipt = json!({"receipt_key":IDEM,"state":"succeeded",
            "outcome":{"code":"continuation_established"}})
        .to_string()
        .into_bytes();
        *lock(&rig.fake.answer) = Ok(receipt.clone());
        let reply = rig
            .post(&req, &body, &policy(true), Duration::from_secs(2))
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, receipt);
        assert_eq!(rig.fake.answers(), 2);
        assert_eq!(rig.fake.targets(), 0);
        assert_ne!(
            lock(&rig.fake.answer_calls)[1]["origin"]["gateway_epoch"],
            original_epoch
        );
    }

    #[tokio::test]
    async fn pending_answer_uses_daemon_target_fence_and_strict_body() {
        const PUBLICATION: &str = "550e8400-e29b-41d4-a716-446655440010";
        let body = json!({"decision_id":format!("pending-question:{PUBLICATION}"),"expected_target_digest":"digest-pending", "answer":"Proceed", "idempotency_key":"550e8400-e29b-41d4-a716-446655440011"});
        let rig = Rig::new();
        *lock(&rig.fake.answer) = Err(OperatorError::Refused("decision_changed".into()));
        let mut req = rig.request();
        req.path = req.path.replace("/answer", "/answer-pending");
        let denied = rig
            .post(
                &req,
                body.to_string().as_bytes(),
                &policy(true),
                Duration::ZERO,
            )
            .await;
        assert_eq!(denied.status, 409);
        assert_eq!(rig.fake.answers(), 1);
        for variant in [
            json!([]),
            json!({"extra":1}),
            {
                let mut v = body.clone();
                v["origin"] = json!({"client_node":"forged"});
                v
            },
            {
                let mut v = body.clone();
                v["answer"] = json!("a\0b");
                v
            },
            {
                let mut v = body.clone();
                v["answer"] = json!("x".repeat(2049));
                v
            },
        ] {
            let rig = Rig::new();
            let mut req = rig.request();
            req.path = req.path.replace("/answer", "/answer-pending");
            let denied = rig
                .post(
                    &req,
                    variant.to_string().as_bytes(),
                    &policy(true),
                    Duration::ZERO,
                )
                .await;
            assert_eq!(denied.status, 400);
            assert_eq!(rig.fake.answers(), 0);
        }
        let rig = Rig::new();
        let mut req = rig.request();
        req.path = req.path.replace("/answer", "/answer-pending");
        *lock(&rig.fake.targets) = Ok(json!({"manager":"not_configured","fence":null,"items":[], "pending_items":[{
            "decision_id":format!("pending-question:{PUBLICATION}"),"target_digest":"digest-pending","kind":"question","title":"Proceed?","detail":"","options":[],"receipt":null
        }], "truncated":false}).to_string().into_bytes());
        *lock(&rig.fake.answer) = Ok(json!({"receipt_key":"550e8400-e29b-41d4-a716-446655440011","state":"queued","outcome":null}).to_string().into_bytes());
        assert_eq!(
            rig.post(
                &req,
                body.to_string().as_bytes(),
                &policy(true),
                Duration::ZERO
            )
            .await
            .status,
            200
        );
        assert_eq!(rig.fake.answers(), 1);
        assert_eq!(rig.audit.lines().len(), 2);
        assert_eq!(
            lock(&rig.fake.answer_calls)[0]["origin"]["client_node"],
            "nCLIENT"
        );
    }
}
