//! RSI Remote API v2 (#2962): the exposed projects' manager seats, a manager
//! seat's conversation and one second closed write, a message to a seat.
//!
//! The GETs are read-only: they need a bound session (cookie plus the Origin
//! check the V1 reads use) and project scope, and they dispatch only the
//! closed `OperatorCall` variants below. The dashboard (hub #3287) names no
//! project: it carries fleet-wide satellite labels and agent counts only, so
//! the bound session is its whole scope.
//!
//! `MessageGate::serve` is a closed pipeline modelled on `AnswerGate`. Every
//! step before the final daemon call is a denial that dispatches nothing to
//! the message method:
//!
//! 1. exact `Origin`, bound session cookie and `x-rsi-csrf`;
//! 2. the `allow_messages` switch and project scope, read from the policy of
//!    THIS request;
//! 3. per-session rate limit (1 per second, 32 per hour, separate from
//!    answers);
//! 4. a typed body that denies unknown fields, text 1..=4096 bytes without
//!    NUL and not blank, idempotency key a canonical UUID;
//! 5. a fresh daemon read of the project's manager seats must name the
//!    target as the project manager or the covering global seat (the daemon
//!    proves it again);
//! 6. an `attempt` audit line is durably written (else 503, nothing sent);
//! 7. one `RemoteSendManagerMessageV2` call, then an `outcome` audit line.
//!
//! The audit trail never holds the text, only its SHA-256 and length. A
//! transport failure after the request line may have been written is
//! recorded `unknown`, never as success; the client retries with the same
//! idempotency key and the daemon replays the queued row (`QueueOperatorMessage`
//! is idempotent per session and key, delivery is at most once, #945).

use crate::{
    answers::{
        AnswerReply, Forwarded, RETRY_AFTER_BUSY, RateLimiter, lock, now_rfc3339, sha256_hex,
    },
    config::Config,
    ingress::{self, Method, Request},
    reads::{OperatorCall, OperatorDispatch, OperatorError, ReadError},
    session::{self, Binding, SessionDenial, Sessions},
};
use rsi_common::remote_manager_v2::{
    CONVERSATION_PAGE_LIMIT, ConversationAnchorV2, ConversationRequestV2, DashboardRequestV2,
    ListManagerSeatsRequestV2, ListManagerSeatsResponseV2, ProjectSeatsV2, ProvidersRequestV2,
    REFUSAL_IDEMPOTENCY_CONFLICT, REFUSAL_INVALID_MESSAGE, REFUSAL_SEAT_NOT_LIVE,
    REFUSAL_SEAT_OUT_OF_SCOPE, SendMessageReceiptV2, SendMessageRequestV2, valid_message_text,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use uuid::Uuid;

/// Seconds a phone is asked to wait after a busy or timed-out v2 read.
const READ_RETRY_AFTER_SECS: u64 = 2;

fn uuid_of(text: &str) -> Option<Uuid> {
    ingress::is_canonical_uuid(text)
        .then(|| Uuid::parse_str(text).ok())
        .flatten()
}

/// The wire request of the project list: every project the policy exposes.
fn list_call(policy: &Config) -> Option<OperatorCall> {
    let project_ids = policy
        .project_ids
        .iter()
        .map(|id| uuid_of(id))
        .collect::<Option<Vec<_>>>()?;
    Some(OperatorCall::ManagerSeats(ListManagerSeatsRequestV2 {
        project_ids,
    }))
}

/// The call for a v2 GET path, or the status that denies it before any
/// dispatch. A project outside `policy.project_ids` is a bare 403, exactly as
/// the V1 reads deny it.
fn read_call(req: &Request, policy: &Config) -> Result<OperatorCall, AnswerReply> {
    let Some(method) = ingress::v2_read_method(&req.path) else {
        return Err(AnswerReply::bare(404));
    };
    if method == "RemoteListManagerSeatsV2" {
        return list_call(policy).ok_or_else(|| AnswerReply::bare(403));
    }
    if method == "RemoteGetDashboardV2" {
        // Fleet-wide labels and counts; no project is named, so no project
        // scope applies beyond the bound session. Any query is refused.
        if req.query != ingress::Query::default() {
            return Err(AnswerReply::coded(400, "invalid_request"));
        }
        return Ok(OperatorCall::Dashboard(DashboardRequestV2 {}));
    }
    if method == "RemoteGetProvidersV2" {
        // Fleet-wide labels and numbers (hub #3796); no project is named and
        // no query is accepted.
        if req.query != ingress::Query::default() {
            return Err(AnswerReply::coded(400, "invalid_request"));
        }
        return Ok(OperatorCall::Providers(ProvidersRequestV2 {}));
    }
    let segments: Vec<&str> = req.path.split('/').collect();
    let (Some(project), Some(seat)) = (segments.get(4), segments.get(6)) else {
        return Err(AnswerReply::bare(404));
    };
    if !policy.project_ids.iter().any(|allowed| allowed == project) {
        return Err(AnswerReply::bare(403));
    }
    let (Some(project_id), Some(seat_session_id)) = (uuid_of(project), uuid_of(seat)) else {
        return Err(AnswerReply::bare(403));
    };
    Ok(OperatorCall::ManagerConversation(ConversationRequestV2 {
        project_id,
        seat_session_id,
        before: req
            .query
            .before
            .map(|(sequence, id)| ConversationAnchorV2 { sequence, id }),
        limit: CONVERSATION_PAGE_LIMIT,
    }))
}

/// The phone-facing body of `GET /api/v2/projects` (hub #2988): the daemon's
/// seat list plus `messages_enabled`, this request's policy `allow_messages`,
/// so a client can show messaging as off before it sends. Read-only and
/// additive: a send while it is `false` is still the bare `403`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectsResponseV2 {
    pub projects: Vec<ProjectSeatsV2>,
    pub messages_enabled: bool,
}

/// The 200 body of the project list, built from the daemon's seat list. A
/// daemon reply that is not that wire type is a malformed read.
fn projects_body(bytes: &[u8], policy: &Config) -> Result<Vec<u8>, ReadError> {
    let seats: ListManagerSeatsResponseV2 =
        serde_json::from_slice(bytes).map_err(|_| ReadError::Malformed)?;
    serde_json::to_vec(&ProjectsResponseV2 {
        projects: seats.projects,
        messages_enabled: policy.allow_messages,
    })
    .map_err(|_| ReadError::Malformed)
}

fn failed_read(error: ReadError) -> AnswerReply {
    let status = error.http_status();
    let reply = AnswerReply::coded(status, error.code());
    if status == 503 {
        reply.after(READ_RETRY_AFTER_SECS)
    } else {
        reply
    }
}

/// Serve one v2 GET: session check, scope, one closed daemon read.
pub async fn serve_read<D: OperatorDispatch>(
    req: &Request,
    origin_ok: bool,
    binding: &Binding,
    sessions: &Sessions,
    policy: &Config,
    dispatch: &D,
    now: Instant,
) -> AnswerReply {
    if req.method != Method::Get {
        return AnswerReply::bare(404);
    }
    let cookie = session::cookie(req.cookie.as_deref(), session::SESSION_COOKIE);
    if sessions.check(cookie, origin_ok, binding, now).is_err() {
        return AnswerReply::bare(401);
    }
    let call = match read_call(req, policy) {
        Ok(call) => call,
        Err(reply) => return reply,
    };
    let is_list = matches!(call, OperatorCall::ManagerSeats(_));
    match dispatch.call(&call).await {
        Ok(bytes) => {
            let body = if is_list {
                match projects_body(&bytes, policy) {
                    Ok(body) => body,
                    Err(error) => return failed_read(error),
                }
            } else {
                bytes
            };
            AnswerReply {
                status: 200,
                body,
                retry_after: None,
            }
        }
        Err(OperatorError::Refused(message)) => match message.as_str() {
            // The daemon proved the session is no manager seat of the
            // project (or the page anchor is gone).
            "not_found" => AnswerReply::coded(404, "not_found"),
            "invalid_request" => AnswerReply::coded(400, "invalid_request"),
            _ => AnswerReply::bare(403),
        },
        Err(OperatorError::Transport(error)) => failed_read(error),
    }
}

/// The wire body of a message POST: an object with exactly these fields.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageBody {
    text: String,
    idempotency_key: String,
}

fn parse_body(body: &[u8]) -> Option<(String, Uuid)> {
    let value: Value = serde_json::from_slice(body).ok()?;
    // Derived struct deserialization also accepts positional arrays; the wire
    // contract is objects only.
    if !value.is_object() {
        return None;
    }
    let parsed: MessageBody = serde_json::from_value(value).ok()?;
    let key = uuid_of(&parsed.idempotency_key)?;
    valid_message_text(&parsed.text).then_some((parsed.text, key))
}

/// What happened to the forwarded message, as far as the gateway can know.
enum Outcome {
    Receipt(SendMessageReceiptV2),
    /// The daemon refused; the closed code, when it named one.
    Refused(Option<&'static str>),
    /// The call never reached the socket (all permits taken).
    NotSent,
    /// The request line may have been written: delivery is unknown.
    Unknown(ReadError),
}

impl Outcome {
    const fn result(&self) -> &'static str {
        match self {
            Self::Receipt(_) => "receipt",
            Self::Refused(_) => "refused",
            Self::NotSent => "not_sent",
            Self::Unknown(_) => "unknown",
        }
    }
}

fn refusal_code(message: &str) -> Option<&'static str> {
    [
        REFUSAL_SEAT_OUT_OF_SCOPE,
        REFUSAL_INVALID_MESSAGE,
        REFUSAL_IDEMPOTENCY_CONFLICT,
        REFUSAL_SEAT_NOT_LIVE,
    ]
    .into_iter()
    .find(|code| message.contains(code))
}

fn classify(result: Result<Vec<u8>, OperatorError>) -> (Outcome, Vec<u8>) {
    match result {
        Ok(bytes) => {
            // Success needs a well-formed receipt. Any other object means the
            // daemon's outcome cannot be established: it is unknown, so the
            // same-key retry stays possible.
            match serde_json::from_slice::<SendMessageReceiptV2>(&bytes) {
                Ok(receipt) => (Outcome::Receipt(receipt), bytes),
                Err(_) => (Outcome::Unknown(ReadError::Malformed), Vec::new()),
            }
        }
        Err(OperatorError::Refused(message)) => {
            (Outcome::Refused(refusal_code(&message)), Vec::new())
        }
        Err(OperatorError::Transport(ReadError::Busy)) => (Outcome::NotSent, Vec::new()),
        Err(OperatorError::Transport(error)) => (Outcome::Unknown(error), Vec::new()),
    }
}

/// Everything about one message request that is not gateway-wide state.
pub struct MessageRequest<'a> {
    pub req: &'a Request,
    pub body: &'a [u8],
    pub origin_ok: bool,
    pub binding: &'a Binding,
    pub sessions: &'a Sessions,
    pub policy: &'a Config,
    pub now: Instant,
}

/// Gateway-wide message state: the shared audit sink, the rate limiter and the
/// bounded memory of forwarded requests.
pub struct MessageGate {
    audit: Arc<dyn crate::answers::AuditSink>,
    limiter: Mutex<RateLimiter>,
    forwarded: Mutex<Forwarded>,
}

impl MessageGate {
    #[must_use]
    pub fn new(audit: Arc<dyn crate::answers::AuditSink>) -> Self {
        Self {
            audit,
            limiter: Mutex::new(RateLimiter::default()),
            forwarded: Mutex::new(Forwarded::default()),
        }
    }

    /// Run the whole message pipeline for one message POST.
    pub async fn serve<D: OperatorDispatch>(
        &self,
        input: &MessageRequest<'_>,
        dispatch: &D,
    ) -> AnswerReply {
        let MessageRequest {
            req,
            body,
            origin_ok,
            binding,
            sessions,
            policy,
            now,
        } = *input;
        let Some((project, seat)) = ingress::message_route(&req.path) else {
            return AnswerReply::bare(404);
        };
        let (project, seat) = (project.as_str(), seat.as_str());
        if req.method != Method::Post {
            return AnswerReply::bare(404);
        }
        // A message needs the Origin header itself, not the Fetch Metadata
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
        if !policy.allow_messages || !policy.project_ids.iter().any(|allowed| allowed == project) {
            return AnswerReply::bare(403);
        }
        let limited = lock(&self.limiter).check(cookie, now);
        if let Err(wait) = limited {
            return AnswerReply::coded(429, "rate_limited")
                .after(wait.as_secs() + u64::from(wait.subsec_nanos() > 0));
        }
        let Some((text, key)) = parse_body(body) else {
            return AnswerReply::coded(400, "invalid_message");
        };
        let (Some(project_id), Some(seat_session_id)) = (uuid_of(project), uuid_of(seat)) else {
            return AnswerReply::coded(400, "invalid_message");
        };
        let call = OperatorCall::SendManagerMessage(SendMessageRequestV2 {
            project_id,
            seat_session_id,
            text: text.clone(),
            idempotency_key: key,
        });
        let payload = sha256_hex(
            &serde_json::to_vec(&json!([project, seat, sha256_hex(text.as_bytes())]))
                .unwrap_or_default(),
        );
        let key_text = key.to_string();
        let replay = lock(&self.forwarded).is_repeat(&key_text, &payload, now);

        // The target must be a manager seat of this exposed project right now.
        if let Some(reply) = Self::check_seat(project_id, seat_session_id, dispatch).await {
            return reply;
        }

        let session_digest = &sha256_hex(cookie.as_bytes())[..16];
        let common = |phase: &str| {
            json!({
                "ts": now_rfc3339(),
                "phase": phase,
                "kind": "message",
                "node": binding.client_node,
                "session_digest": session_digest,
                "project_id": project,
                "seat_session_id": seat,
                "owner": binding.owner_user_id,
                "idempotency_key": key_text,
                "text_sha256": sha256_hex(text.as_bytes()),
                "text_len": text.len(),
                "replay": replay,
            })
        };
        if self.audit.append(&common("attempt")).is_err() {
            eprintln!("rsi-remote: message refused: audit unavailable");
            return AnswerReply::coded(503, "audit_unavailable").after(RETRY_AFTER_BUSY);
        }

        let (outcome, receipt) = classify(dispatch.call(&call).await);
        let mut line = common("outcome");
        line["result"] = json!(outcome.result());
        let reply = match &outcome {
            Outcome::Receipt(parsed) => {
                lock(&self.forwarded).remember(&key_text, payload, now);
                line["message_id"] = json!(parsed.message_id);
                line["state"] = json!(parsed.state);
                line["deduplicated"] = json!(parsed.deduplicated);
                AnswerReply {
                    status: 200,
                    body: receipt,
                    retry_after: None,
                }
            }
            Outcome::Refused(code) => {
                line["daemon_code"] = json!(code);
                match code {
                    Some(c) if *c == REFUSAL_IDEMPOTENCY_CONFLICT => {
                        AnswerReply::coded(409, REFUSAL_IDEMPOTENCY_CONFLICT)
                    }
                    Some(c) if *c == REFUSAL_SEAT_NOT_LIVE => {
                        AnswerReply::coded(409, REFUSAL_SEAT_NOT_LIVE)
                    }
                    Some(c) if *c == REFUSAL_INVALID_MESSAGE => {
                        AnswerReply::coded(400, "invalid_message")
                    }
                    _ => AnswerReply::bare(403),
                }
            }
            Outcome::NotSent => AnswerReply::coded(503, "busy").after(RETRY_AFTER_BUSY),
            Outcome::Unknown(error) => {
                lock(&self.forwarded).remember(&key_text, payload, now);
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
            // The message is already decided; nothing can be undone. Say so on
            // the host log instead of changing the reply.
            eprintln!("rsi-remote: message outcome audit line not written");
        }
        reply
    }

    /// `None` when a fresh daemon read of the project's seats names `seat` as
    /// the project manager or the covering global seat.
    async fn check_seat<D: OperatorDispatch>(
        project: Uuid,
        seat: Uuid,
        dispatch: &D,
    ) -> Option<AnswerReply> {
        let call = OperatorCall::ManagerSeats(ListManagerSeatsRequestV2 {
            project_ids: vec![project],
        });
        match dispatch.call(&call).await {
            Ok(bytes) => match serde_json::from_slice::<ListManagerSeatsResponseV2>(&bytes) {
                Ok(response) => {
                    let is_seat = response.projects.iter().any(|row| {
                        row.project_id == project
                            && [&row.project_manager, &row.global_manager]
                                .into_iter()
                                .flatten()
                                .any(|candidate| candidate.session_id == seat)
                    });
                    if is_seat {
                        None
                    } else {
                        Some(AnswerReply::bare(403))
                    }
                }
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
pub(crate) mod tests {
    use super::*;
    use crate::answers::{AuditSink, MIN_ANSWER_GAP, MemoryAudit};
    use rsi_common::remote_manager_v2::{
        ConversationItemV2, ConversationResponseV2, DashboardAgentGroupV2, DashboardResponseV2,
        DashboardSatelliteV2, ItemKindV2, ManagerSeatV2, OperatorMessageStateV2, ProjectSeatsV2,
        ProviderModelSpendV2, ProviderRowV2, ProviderSpendV2, ProviderWindowV2,
        ProvidersResponseV2, SeatRoleV2, SpeakerV2,
    };
    use std::time::Duration;

    pub(crate) const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    pub(crate) const OTHER_PROJECT: &str = "550e8400-e29b-41d4-a716-4466554400ff";
    pub(crate) const PM_SEAT: &str = "550e8400-e29b-41d4-a716-446655440001";
    pub(crate) const GLOBAL_SEAT: &str = "550e8400-e29b-41d4-a716-446655440002";
    pub(crate) const WORKER: &str = "550e8400-e29b-41d4-a716-446655440003";
    pub(crate) const IDEM: &str = "550e8400-e29b-41d4-a716-446655440009";
    pub(crate) const MESSAGE_ID: &str = "550e8400-e29b-41d4-a716-44665544000a";
    pub(crate) const HOST: &str = "host.example.ts.net";
    const TEXT: &str = "please prioritize the migration, tell nobody secret-token-text";

    fn uuid(text: &str) -> Uuid {
        Uuid::parse_str(text).unwrap()
    }

    pub(crate) fn policy(allow_messages: bool) -> Config {
        Config {
            enabled: true,
            canonical_host: HOST.to_string(),
            owner_user_id: 501,
            allowed_node_ids: vec!["nHOME".to_string()],
            project_ids: vec![PROJECT.to_string()],
            allow_answers: false,
            allow_messages,
        }
    }

    pub(crate) fn binding() -> Binding {
        Binding {
            owner_user_id: 501,
            client_node: "nCLIENT".to_string(),
            server_node: "nHOME".to_string(),
        }
    }

    pub(crate) fn seat(session: &str, role: SeatRoleV2) -> ManagerSeatV2 {
        ManagerSeatV2 {
            session_id: uuid(session),
            role,
            status: "Running".into(),
            provider: "Claude".into(),
            model: Some("claude-opus-5-5".into()),
            last_activity_at: "2026-10-17T09:30:00.000000000Z".into(),
            pending_decisions: (role == SeatRoleV2::ProjectManager).then_some(2),
            pending_decisions_truncated: false,
        }
    }

    pub(crate) fn seats_body(with_global: bool) -> Vec<u8> {
        serde_json::to_vec(&ListManagerSeatsResponseV2 {
            projects: vec![ProjectSeatsV2 {
                project_id: uuid(PROJECT),
                name: "Alpha".into(),
                project_manager: Some(seat(PM_SEAT, SeatRoleV2::ProjectManager)),
                global_manager: with_global.then(|| seat(GLOBAL_SEAT, SeatRoleV2::GlobalManager)),
            }],
        })
        .unwrap()
    }

    pub(crate) fn receipt_body(deduplicated: bool) -> Vec<u8> {
        serde_json::to_vec(&SendMessageReceiptV2 {
            message_id: uuid(MESSAGE_ID),
            seat_session_id: uuid(PM_SEAT),
            state: "queued".into(),
            created_at: "2026-10-17T09:31:00.000000000Z".into(),
            deduplicated,
        })
        .unwrap()
    }

    pub(crate) fn conversation_body(first_page: bool) -> Vec<u8> {
        let item = |id: &str, sequence: i32, speaker: SpeakerV2, text: &str| ConversationItemV2 {
            id: id.into(),
            sequence,
            speaker,
            text: text.into(),
            truncated: false,
            created_at: format!("2026-10-17T09:2{sequence}:00.000000000Z"),
            operator_message_id: (speaker == SpeakerV2::Operator).then(|| uuid(MESSAGE_ID)),
            kind: ItemKindV2::Message,
            tool_name: None,
            tool_use_id: None,
            idempotency_key: None,
        };
        let response = if first_page {
            ConversationResponseV2 {
                seat: seat(PM_SEAT, SeatRoleV2::ProjectManager),
                items: vec![
                    item(
                        "904",
                        4,
                        SpeakerV2::Manager,
                        "Migration reserved; landing next.",
                    ),
                    item(
                        "903",
                        3,
                        SpeakerV2::Operator,
                        "Please prioritize the migration.",
                    ),
                ],
                next_before: Some("3:903".into()),
                has_more: true,
                operator_messages: vec![OperatorMessageStateV2 {
                    message_id: uuid(MESSAGE_ID),
                    state: "delivered".into(),
                    created_at: "2026-10-17T09:23:00.000000000Z".into(),
                    updated_at: "2026-10-17T09:23:05.000000000Z".into(),
                    delivered_at: Some("2026-10-17T09:23:05.000000000Z".into()),
                    text_len: 32,
                    idempotency_key: None,
                }],
            }
        } else {
            ConversationResponseV2 {
                seat: seat(PM_SEAT, SeatRoleV2::ProjectManager),
                items: vec![item("902", 2, SpeakerV2::System, "Wake: 3 workers idle.")],
                next_before: None,
                has_more: false,
                operator_messages: Vec::new(),
            }
        };
        serde_json::to_vec(&response).unwrap()
    }

    pub(crate) fn dashboard_body(populated: bool) -> Vec<u8> {
        let response = if populated {
            DashboardResponseV2 {
                observed_at: "2026-10-19T19:30:00.000000000Z".into(),
                satellites: vec![
                    DashboardSatelliteV2 {
                        label: "laptop".into(),
                        reachable: false,
                        build_sha: None,
                        last_seen_at: Some("2026-10-19T18:02:11.000000000Z".into()),
                        running_agents: None,
                    },
                    DashboardSatelliteV2 {
                        label: "mac-mini".into(),
                        reachable: true,
                        build_sha: Some("a077ca3b02ae7d9de512c24d56a5803d88f703e5".into()),
                        last_seen_at: Some("2026-10-19T19:29:40.000000000Z".into()),
                        running_agents: Some(3),
                    },
                ],
                satellites_truncated: false,
                agents: vec![
                    DashboardAgentGroupV2 {
                        provider: "Claude".into(),
                        effort: Some("high".into()),
                        running: 4,
                    },
                    DashboardAgentGroupV2 {
                        provider: "Codex".into(),
                        effort: Some("xhigh".into()),
                        running: 2,
                    },
                    DashboardAgentGroupV2 {
                        provider: "Claude".into(),
                        effort: None,
                        running: 1,
                    },
                ],
                agents_total: 7,
                agents_truncated: false,
                agents_incomplete: false,
            }
        } else {
            DashboardResponseV2 {
                observed_at: "2026-10-19T19:30:00.000000000Z".into(),
                satellites: Vec::new(),
                satellites_truncated: false,
                agents: Vec::new(),
                agents_total: 0,
                agents_truncated: false,
                agents_incomplete: false,
            }
        };
        serde_json::to_vec(&response).unwrap()
    }

    fn provider_row(provider: &str) -> ProviderRowV2 {
        ProviderRowV2 {
            provider: provider.into(),
            status: None,
            binding_window: None,
            overage_status: None,
            is_using_overage: false,
            observed_at: None,
            stale_reason: None,
            windows: Vec::new(),
            windows_reason: Some("no_observation".into()),
            windows_truncated: false,
            spend: None,
            spend_reason: Some("warming".into()),
            models: Vec::new(),
            models_truncated: false,
        }
    }

    /// The providers body: a populated Claude (two windows, spend, two models),
    /// a stale Codex, and a provider with no data (`null` blocks with reasons);
    /// or only no-data providers.
    pub(crate) fn providers_body(populated: bool) -> Vec<u8> {
        let mut providers = vec![provider_row("Claude"), provider_row("Codex")];
        if populated {
            providers[0] = ProviderRowV2 {
                status: Some("allowed".into()),
                binding_window: Some("five_hour".into()),
                overage_status: Some("rejected".into()),
                observed_at: Some("2026-10-22T12:00:00.000000000Z".into()),
                windows: vec![
                    ProviderWindowV2 {
                        key: "five_hour".into(),
                        utilization: 0.42,
                        resets_at: Some("2026-10-22T15:00:00.000000000Z".into()),
                        resets_at_epoch: Some(1_792_681_200),
                    },
                    ProviderWindowV2 {
                        key: "seven_day".into(),
                        utilization: 0.1,
                        resets_at: None,
                        resets_at_epoch: None,
                    },
                ],
                windows_reason: None,
                spend: Some(ProviderSpendV2 {
                    cost_1h_usd: 1.25,
                    cost_24h_usd: 9.5,
                }),
                spend_reason: None,
                models: vec![
                    ProviderModelSpendV2 {
                        model: "claude-opus".into(),
                        cost_1h_usd: 1.0,
                        cost_24h_usd: 8.0,
                    },
                    ProviderModelSpendV2 {
                        model: "claude-haiku".into(),
                        cost_1h_usd: 0.25,
                        cost_24h_usd: 1.5,
                    },
                ],
                ..provider_row("Claude")
            };
            providers[1] = ProviderRowV2 {
                status: Some("allowed_warning".into()),
                binding_window: Some("weekly".into()),
                overage_status: None,
                observed_at: Some("2026-10-22T11:58:00.000000000Z".into()),
                stale_reason: Some("probe_timeout".into()),
                windows: vec![ProviderWindowV2 {
                    key: "weekly".into(),
                    utilization: 0.97,
                    resets_at: Some("2026-10-25T00:00:00.000000000Z".into()),
                    resets_at_epoch: Some(1_792_886_400),
                }],
                windows_reason: None,
                spend: Some(ProviderSpendV2 {
                    cost_1h_usd: 0.0,
                    cost_24h_usd: 0.0,
                }),
                spend_reason: None,
                ..provider_row("Codex")
            };
        }
        serde_json::to_vec(&ProvidersResponseV2 {
            observed_at: "2026-10-22T12:00:00.000000000Z".into(),
            providers,
            providers_truncated: false,
            windows_incomplete: false,
        })
        .unwrap()
    }

    /// A hub running more agents than the dashboard counts: the read stopped
    /// at the cap, so the total is the cap and the reply says incomplete.
    pub(crate) fn dashboard_incomplete_body() -> Vec<u8> {
        let cap =
            u32::try_from(rsi_common::remote_manager_v2::DASHBOARD_MAX_RUNNING_AGENTS).unwrap();
        let response = DashboardResponseV2 {
            observed_at: "2026-10-19T19:30:00.000000000Z".into(),
            satellites: Vec::new(),
            satellites_truncated: false,
            agents: vec![
                DashboardAgentGroupV2 {
                    provider: "Claude".into(),
                    effort: Some("high".into()),
                    running: cap - 24,
                },
                DashboardAgentGroupV2 {
                    provider: "Codex".into(),
                    effort: Some("xhigh".into()),
                    running: 24,
                },
            ],
            agents_total: cap,
            agents_truncated: false,
            agents_incomplete: true,
        };
        serde_json::to_vec(&response).unwrap()
    }

    /// Scripted daemon: answers each v2 method from a slot and records calls.
    pub(crate) struct Fake {
        pub(crate) seats: Mutex<Result<Vec<u8>, OperatorError>>,
        pub(crate) dashboard: Mutex<Result<Vec<u8>, OperatorError>>,
        pub(crate) providers: Mutex<Result<Vec<u8>, OperatorError>>,
        pub(crate) conversation: Mutex<Result<Vec<u8>, OperatorError>>,
        pub(crate) send: Mutex<Result<Vec<u8>, OperatorError>>,
        pub(crate) calls: Mutex<Vec<(String, Value)>>,
    }

    impl Fake {
        pub(crate) fn new() -> Self {
            Self {
                seats: Mutex::new(Ok(seats_body(true))),
                dashboard: Mutex::new(Ok(dashboard_body(true))),
                providers: Mutex::new(Ok(providers_body(true))),
                conversation: Mutex::new(Ok(conversation_body(true))),
                send: Mutex::new(Ok(receipt_body(false))),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn count(&self, method: &str) -> usize {
            lock(&self.calls)
                .iter()
                .filter(|(m, _)| m == method)
                .count()
        }

        pub(crate) fn total(&self) -> usize {
            lock(&self.calls).len()
        }

        pub(crate) fn sends(&self) -> Vec<Value> {
            lock(&self.calls)
                .iter()
                .filter(|(m, _)| m == "RemoteSendManagerMessageV2")
                .map(|(_, params)| params.clone())
                .collect()
        }
    }

    impl OperatorDispatch for Fake {
        async fn call(&self, call: &OperatorCall) -> Result<Vec<u8>, OperatorError> {
            let wire = call.wire().unwrap();
            lock(&self.calls).push((call.method().to_string(), wire["params"].clone()));
            match call {
                OperatorCall::ManagerSeats(_) => lock(&self.seats).clone(),
                OperatorCall::ManagerConversation(_) => lock(&self.conversation).clone(),
                OperatorCall::SendManagerMessage(_) => lock(&self.send).clone(),
                OperatorCall::Dashboard(_) => lock(&self.dashboard).clone(),
                OperatorCall::Providers(_) => lock(&self.providers).clone(),
                _ => Err(OperatorError::Refused("not a v2 call".into())),
            }
        }
    }

    pub(crate) struct Rig {
        pub(crate) sessions: Sessions,
        pub(crate) gate: MessageGate,
        pub(crate) audit: Arc<MemoryAudit>,
        pub(crate) fake: Fake,
        pub(crate) cookie: String,
        pub(crate) csrf: String,
        pub(crate) now: Instant,
    }

    impl Rig {
        pub(crate) fn new() -> Self {
            let now = Instant::now();
            let sessions = Sessions::new();
            let boot = sessions.bootstrap(&binding(), now).unwrap();
            let established = sessions
                .establish(Some(&boot.cookie), &boot.secret, true, &binding(), now)
                .unwrap();
            let audit = Arc::new(MemoryAudit::default());
            Self {
                sessions,
                gate: MessageGate::new(audit.clone()),
                audit,
                fake: Fake::new(),
                cookie: established.cookie,
                csrf: established.secret,
                now,
            }
        }

        pub(crate) fn get(&self, path: &str, query: ingress::Query) -> Request {
            Request {
                method: Method::Get,
                path: path.to_string(),
                query,
                source: "100.101.102.103".parse().unwrap(),
                content_length: 0,
                origin: Some(format!("https://{HOST}")),
                sec_fetch_site: None,
                cookie: Some(format!("{}={}", session::SESSION_COOKIE, self.cookie)),
                csrf: None,
            }
        }

        pub(crate) fn request(&self) -> Request {
            Request {
                method: Method::Post,
                path: format!("/api/v2/projects/{PROJECT}/managers/{PM_SEAT}/messages"),
                query: ingress::Query::default(),
                source: "100.101.102.103".parse().unwrap(),
                content_length: 0,
                origin: Some(format!("https://{HOST}")),
                sec_fetch_site: None,
                cookie: Some(format!("{}={}", session::SESSION_COOKIE, self.cookie)),
                csrf: Some(self.csrf.clone()),
            }
        }

        pub(crate) async fn read(&self, req: &Request, policy: &Config) -> AnswerReply {
            let origin_ok =
                session::origin_ok(req.origin.as_deref(), req.sec_fetch_site.as_deref(), HOST);
            serve_read(
                req,
                origin_ok,
                &binding(),
                &self.sessions,
                policy,
                &self.fake,
                self.now,
            )
            .await
        }

        pub(crate) async fn post(
            &self,
            req: &Request,
            body: &[u8],
            policy: &Config,
            at: Duration,
        ) -> AnswerReply {
            let origin_ok =
                session::origin_ok(req.origin.as_deref(), req.sec_fetch_site.as_deref(), HOST);
            let input = MessageRequest {
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

        pub(crate) async fn send(&self) -> AnswerReply {
            self.post(
                &self.request(),
                &body_json(TEXT),
                &policy(true),
                Duration::ZERO,
            )
            .await
        }
    }

    pub(crate) fn body_json(text: &str) -> Vec<u8> {
        json!({"text": text, "idempotency_key": IDEM})
            .to_string()
            .into_bytes()
    }

    fn assert_denied_without_dispatch(rig: &Rig, reply: &AnswerReply, status: u16) {
        assert_eq!(reply.status, status);
        assert_eq!(rig.fake.total(), 0, "nothing reached the daemon");
        assert!(rig.audit.lines().is_empty(), "no audit line");
    }

    #[tokio::test]
    async fn project_list_is_the_policy_projects_with_and_without_a_global_seat() {
        let rig = Rig::new();
        let list = rig.get("/api/v2/projects", ingress::Query::default());

        let reply = rig.read(&list, &policy(false)).await;
        assert_eq!(reply.status, 200);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["projects"][0]["project_id"], PROJECT);
        assert_eq!(
            body["projects"][0]["project_manager"]["session_id"],
            PM_SEAT
        );
        assert_eq!(
            body["projects"][0]["project_manager"]["pending_decisions"],
            2
        );
        assert_eq!(
            body["projects"][0]["global_manager"]["session_id"],
            GLOBAL_SEAT
        );
        assert_eq!(
            lock(&rig.fake.calls)[0],
            (
                "RemoteListManagerSeatsV2".to_string(),
                json!({"project_ids": [PROJECT]})
            ),
            "the daemon is asked only for the policy's projects"
        );

        *lock(&rig.fake.seats) = Ok(seats_body(false));
        let reply = rig.read(&list, &policy(false)).await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert!(body["projects"][0]["global_manager"].is_null());
        assert_eq!(
            body["projects"][0]["project_manager"]["session_id"],
            PM_SEAT
        );

        *lock(&rig.fake.seats) = Ok(br#"{"projects":[]}"#.to_vec());
        let reply = rig.read(&list, &policy(false)).await;
        assert_eq!(reply.status, 200);
        assert_eq!(rig.fake.count("RemoteListManagerSeatsV2"), 3);
    }

    #[tokio::test]
    async fn dashboard_needs_a_session_takes_no_query_and_sends_empty_params() {
        let rig = Rig::new();
        let dashboard = rig.get("/api/v2/dashboard", ingress::Query::default());
        let mut anonymous = dashboard.clone();
        anonymous.cookie = None;
        let reply = rig.read(&anonymous, &policy(false)).await;
        assert_denied_without_dispatch(&rig, &reply, 401);
        let mut foreign = dashboard.clone();
        foreign.origin = Some("https://evil.example".into());
        let reply = rig.read(&foreign, &policy(false)).await;
        assert_denied_without_dispatch(&rig, &reply, 401);
        let queried = rig.get(
            "/api/v2/dashboard",
            ingress::Query {
                cursor: Some("x".into()),
                ..ingress::Query::default()
            },
        );
        let reply = rig.read(&queried, &policy(false)).await;
        assert_denied_without_dispatch(&rig, &reply, 400);

        // Not project-scoped: a policy exposing no project still reads it.
        let mut no_projects = policy(false);
        no_projects.project_ids.clear();
        let reply = rig.read(&dashboard, &no_projects).await;
        assert_eq!(reply.status, 200);
        let typed: DashboardResponseV2 = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(typed.satellites[1].label, "mac-mini");
        assert_eq!(typed.agents_total, 7);
        assert_eq!(
            lock(&rig.fake.calls)[0],
            ("RemoteGetDashboardV2".to_string(), json!({})),
            "the daemon is asked with no parameters"
        );
    }

    #[tokio::test]
    async fn providers_need_a_session_take_no_query_and_send_empty_params() {
        let rig = Rig::new();
        let providers = rig.get("/api/v2/providers", ingress::Query::default());
        let mut anonymous = providers.clone();
        anonymous.cookie = None;
        let reply = rig.read(&anonymous, &policy(false)).await;
        assert_denied_without_dispatch(&rig, &reply, 401);
        let mut foreign = providers.clone();
        foreign.origin = Some("https://evil.example".into());
        let reply = rig.read(&foreign, &policy(false)).await;
        assert_denied_without_dispatch(&rig, &reply, 401);
        let queried = rig.get(
            "/api/v2/providers",
            ingress::Query {
                cursor: Some("x".into()),
                ..ingress::Query::default()
            },
        );
        let reply = rig.read(&queried, &policy(false)).await;
        assert_denied_without_dispatch(&rig, &reply, 400);

        // Not project-scoped: a policy exposing no project still reads it.
        let mut no_projects = policy(false);
        no_projects.project_ids.clear();
        let reply = rig.read(&providers, &no_projects).await;
        assert_eq!(reply.status, 200);
        let typed: ProvidersResponseV2 = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(typed.providers[0].windows[0].key, "five_hour");
        assert_eq!(
            lock(&rig.fake.calls)[0],
            ("RemoteGetProvidersV2".to_string(), json!({})),
            "the daemon is asked with no parameters"
        );
    }

    /// Positive allowlist (AGENTS.md rule 7): the providers body carries exactly
    /// the named label and number fields, and the wire type refuses any other
    /// field (an account, key, URL or path), so the gateway's typed check
    /// never forwards one.
    #[test]
    fn providers_carry_only_the_allowed_fields_and_refuse_extra_ones() {
        let body: Value = serde_json::from_slice(&providers_body(true)).unwrap();
        let keys = |value: &Value| {
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(
            keys(&body),
            [
                "observed_at",
                "providers",
                "providers_truncated",
                "windows_incomplete"
            ]
        );
        assert_eq!(
            keys(&body["providers"][0]),
            [
                "binding_window",
                "is_using_overage",
                "models",
                "models_truncated",
                "observed_at",
                "overage_status",
                "provider",
                "spend",
                "spend_reason",
                "stale_reason",
                "status",
                "windows",
                "windows_reason",
                "windows_truncated"
            ]
        );
        assert_eq!(
            keys(&body["providers"][0]["windows"][0]),
            ["key", "resets_at", "resets_at_epoch", "utilization"]
        );
        assert_eq!(
            keys(&body["providers"][0]["spend"]),
            ["cost_1h_usd", "cost_24h_usd"]
        );
        assert_eq!(
            keys(&body["providers"][0]["models"][0]),
            ["cost_1h_usd", "cost_24h_usd", "model"]
        );
        for place in [
            "",
            "/providers/0",
            "/providers/0/windows/0",
            "/providers/0/spend",
        ] {
            for extra in ["token", "account_id", "email", "api_url", "working_dir"] {
                let mut copy = body.clone();
                let target = if place.is_empty() {
                    &mut copy
                } else {
                    copy.pointer_mut(place).unwrap()
                };
                target[extra] = json!("secret");
                assert!(
                    serde_json::from_value::<ProvidersResponseV2>(copy).is_err(),
                    "{extra} at {place:?} must not be accepted"
                );
            }
        }
    }

    /// The dashboard body carries exactly the named label and count fields,
    /// and the wire type refuses any other field (a link, address, token or
    /// path), so the gateway's typed check never forwards one
    /// (`reads::tests::dashboard_reply_with_an_extra_field_is_malformed`).
    #[test]
    fn dashboard_carries_no_secret_fields_and_refuses_extra_ones() {
        let body: Value = serde_json::from_slice(&dashboard_body(true)).unwrap();
        let keys = |value: &Value| {
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(
            keys(&body),
            [
                "agents",
                "agents_incomplete",
                "agents_total",
                "agents_truncated",
                "observed_at",
                "satellites",
                "satellites_truncated"
            ]
        );
        assert_eq!(
            keys(&body["satellites"][0]),
            [
                "build_sha",
                "label",
                "last_seen_at",
                "reachable",
                "running_agents"
            ]
        );
        assert_eq!(keys(&body["agents"][0]), ["effort", "provider", "running"]);

        for leaked in [
            json!({"token": "secret"}),
            json!({"ssh_target": "operator@192.0.2.44"}),
            json!({"working_dir": "/home/operator"}),
        ] {
            let mut copy = body.clone();
            let (key, value) = leaked.as_object().unwrap().iter().next().unwrap();
            match key.as_str() {
                "token" => copy[key] = value.clone(),
                "ssh_target" => copy["satellites"][0][key] = value.clone(),
                _ => copy["agents"][0][key] = value.clone(),
            }
            assert!(
                serde_json::from_value::<DashboardResponseV2>(copy).is_err(),
                "{key} must not be accepted"
            );
        }
    }

    #[tokio::test]
    async fn project_list_reports_messages_enabled_from_allow_messages() {
        let rig = Rig::new();
        let list = rig.get("/api/v2/projects", ingress::Query::default());
        for allow_messages in [true, false] {
            let reply = rig.read(&list, &policy(allow_messages)).await;
            assert_eq!(reply.status, 200);
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(body["messages_enabled"], json!(allow_messages));
            assert_eq!(body["projects"][0]["project_id"], PROJECT);
            let typed: ProjectsResponseV2 = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(typed.messages_enabled, allow_messages);
        }

        // A daemon reply that is not the seat list is a malformed read, never
        // passed through.
        *lock(&rig.fake.seats) = Ok(br#"{"projects":[],"extra":1}"#.to_vec());
        let reply = rig.read(&list, &policy(true)).await;
        assert_eq!(reply, AnswerReply::coded(500, "malformed"));
    }

    #[tokio::test]
    async fn a_403_body_stays_bare_whatever_allow_messages_says() {
        let rig = Rig::new();
        // Messaging off: the send is the same bare 403, no new field.
        let off = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(false),
                Duration::ZERO,
            )
            .await;
        assert_eq!(off, AnswerReply::bare(403));
        assert!(off.body.is_empty());
        assert_eq!(rig.fake.total(), 0);

        // A project outside the policy: the read is the same bare 403 with
        // messaging on or off.
        for allow_messages in [true, false] {
            let path = format!("/api/v2/projects/{OTHER_PROJECT}/managers/{PM_SEAT}/conversation");
            let reply = rig
                .read(
                    &rig.get(&path, ingress::Query::default()),
                    &policy(allow_messages),
                )
                .await;
            assert_eq!(reply, AnswerReply::bare(403));
        }
        assert_eq!(rig.fake.total(), 0);
    }

    #[tokio::test]
    async fn conversation_pages_walk_back_with_the_before_cursor() {
        let rig = Rig::new();
        let path = format!("/api/v2/projects/{PROJECT}/managers/{PM_SEAT}/conversation");
        let first = rig
            .read(&rig.get(&path, ingress::Query::default()), &policy(false))
            .await;
        assert_eq!(first.status, 200);
        let page: ConversationResponseV2 = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(page.items[0].speaker, SpeakerV2::Manager);
        assert_eq!(page.items[1].speaker, SpeakerV2::Operator);
        assert_eq!(page.operator_messages[0].state, "delivered");
        assert_eq!(
            lock(&rig.fake.calls)[0].1,
            json!({"project_id": PROJECT, "seat_session_id": PM_SEAT, "before": null, "limit": 25})
        );

        // The client hands next_before back as ?before=; the gateway forwards
        // exactly that anchor and no other.
        *lock(&rig.fake.conversation) = Ok(conversation_body(false));
        let (sequence, id) = page
            .next_before
            .unwrap()
            .split_once(':')
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .unwrap();
        let query = ingress::Query {
            before: Some((sequence.parse().unwrap(), id.parse().unwrap())),
            ..ingress::Query::default()
        };
        let older = rig.read(&rig.get(&path, query), &policy(false)).await;
        let page: ConversationResponseV2 = serde_json::from_slice(&older.body).unwrap();
        assert!(!page.has_more && page.next_before.is_none());
        assert_eq!(page.items[0].speaker, SpeakerV2::System);
        assert_eq!(
            lock(&rig.fake.calls)[1].1,
            json!({"project_id": PROJECT, "seat_session_id": PM_SEAT,
                "before": {"sequence": 3, "id": 903}, "limit": 25})
        );
    }

    #[tokio::test]
    async fn a_seat_the_daemon_does_not_know_is_404_and_a_daemon_refusal_is_403() {
        let rig = Rig::new();
        let path = format!("/api/v2/projects/{PROJECT}/managers/{WORKER}/conversation");
        *lock(&rig.fake.conversation) = Err(OperatorError::Refused("not_found".into()));
        let reply = rig
            .read(&rig.get(&path, ingress::Query::default()), &policy(false))
            .await;
        assert_eq!(reply.status, 404);
        *lock(&rig.fake.conversation) = Err(OperatorError::Refused("anything else".into()));
        let reply = rig
            .read(&rig.get(&path, ingress::Query::default()), &policy(false))
            .await;
        assert_eq!(reply.status, 403);
        *lock(&rig.fake.conversation) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig
            .read(&rig.get(&path, ingress::Query::default()), &policy(false))
            .await;
        assert_eq!((reply.status, reply.retry_after), (503, Some(2)));
    }

    #[tokio::test]
    async fn v2_read_denials_dispatch_nothing() {
        let rig = Rig::new();
        let list = rig.get("/api/v2/projects", ingress::Query::default());
        let mut no_cookie = list.clone();
        no_cookie.cookie = None;
        assert_denied_without_dispatch(&rig, &rig.read(&no_cookie, &policy(false)).await, 401);

        let mut wrong_cookie = list.clone();
        wrong_cookie.cookie = Some(format!("{}={}", session::SESSION_COOKIE, "0".repeat(64)));
        assert_denied_without_dispatch(&rig, &rig.read(&wrong_cookie, &policy(false)).await, 401);

        let mut foreign_origin = list.clone();
        foreign_origin.origin = Some("https://evil.example".to_string());
        assert_denied_without_dispatch(&rig, &rig.read(&foreign_origin, &policy(false)).await, 401);

        let out_of_scope = rig.get(
            &format!("/api/v2/projects/{OTHER_PROJECT}/managers/{PM_SEAT}/conversation"),
            ingress::Query::default(),
        );
        assert_denied_without_dispatch(&rig, &rig.read(&out_of_scope, &policy(false)).await, 403);

        let mut post = list;
        post.method = Method::Post;
        assert_denied_without_dispatch(&rig, &rig.read(&post, &policy(false)).await, 404);
    }

    #[tokio::test]
    async fn a_message_is_accepted_and_forwarded_as_the_typed_send_after_a_seat_check() {
        let rig = Rig::new();
        let reply = rig.send().await;
        assert_eq!(reply.status, 200);
        let receipt: SendMessageReceiptV2 = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(receipt.message_id, uuid(MESSAGE_ID));
        assert_eq!(receipt.state, "queued");
        assert!(!receipt.deduplicated);

        let calls = lock(&rig.fake.calls).clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "RemoteListManagerSeatsV2");
        assert_eq!(calls[0].1, json!({"project_ids": [PROJECT]}));
        assert_eq!(calls[1].0, "RemoteSendManagerMessageV2");
        assert_eq!(
            calls[1].1,
            json!({"project_id": PROJECT, "seat_session_id": PM_SEAT,
                "text": TEXT, "idempotency_key": IDEM})
        );

        let lines = rig.audit.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["phase"], "attempt");
        assert_eq!(lines[1]["phase"], "outcome");
        assert_eq!(lines[1]["result"], "receipt");
        assert_eq!(lines[1]["message_id"], MESSAGE_ID);
        assert_eq!(lines[1]["status"], 200);
        for line in &lines {
            assert_eq!(line["kind"], "message");
            assert_eq!(line["node"], "nCLIENT");
            assert_eq!(line["project_id"], PROJECT);
            assert_eq!(line["seat_session_id"], PM_SEAT);
            assert_eq!(line["idempotency_key"], IDEM);
            assert_eq!(line["text_sha256"], sha256_hex(TEXT.as_bytes()));
            assert_eq!(line["text_len"], TEXT.len());
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
    async fn the_covering_global_seat_is_a_valid_target() {
        let rig = Rig::new();
        let mut req = rig.request();
        req.path = format!("/api/v2/projects/{PROJECT}/managers/{GLOBAL_SEAT}/messages");
        let reply = rig
            .post(&req, &body_json("status?"), &policy(true), Duration::ZERO)
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(rig.fake.sends()[0]["seat_session_id"], GLOBAL_SEAT);
    }

    #[tokio::test]
    async fn every_gateway_denial_dispatches_no_message() {
        let rig = Rig::new();
        let body = body_json(TEXT);
        let ok = || rig.request();

        let mut no_cookie = ok();
        no_cookie.cookie = None;
        let reply = rig
            .post(&no_cookie, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 401);

        let mut wrong_cookie = ok();
        wrong_cookie.cookie = Some(format!("{}={}", session::SESSION_COOKIE, "0".repeat(64)));
        let reply = rig
            .post(&wrong_cookie, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 401);

        let mut no_csrf = ok();
        no_csrf.csrf = None;
        let reply = rig
            .post(&no_csrf, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 403);

        let mut bad_csrf = ok();
        bad_csrf.csrf = Some("f".repeat(64));
        let reply = rig
            .post(&bad_csrf, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 403);

        let mut foreign_origin = ok();
        foreign_origin.origin = Some("https://evil.example".to_string());
        let reply = rig
            .post(&foreign_origin, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 403);

        let mut fetch_metadata_only = ok();
        fetch_metadata_only.origin = None;
        fetch_metadata_only.sec_fetch_site = Some("same-origin".to_string());
        let reply = rig
            .post(&fetch_metadata_only, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 403);

        // The switch is read from this request's policy: off denies.
        let reply = rig.post(&ok(), &body, &policy(false), Duration::ZERO).await;
        assert_denied_without_dispatch(&rig, &reply, 403);

        // An unexposed project denies before any seat check.
        let mut other_project = ok();
        other_project.path =
            format!("/api/v2/projects/{OTHER_PROJECT}/managers/{PM_SEAT}/messages");
        let reply = rig
            .post(&other_project, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 403);

        let mut get = ok();
        get.method = Method::Get;
        let reply = rig.post(&get, &body, &policy(true), Duration::ZERO).await;
        assert_denied_without_dispatch(&rig, &reply, 404);

        let mut off_route = ok();
        off_route.path = format!("/api/v2/projects/{PROJECT}/managers/{PM_SEAT}/conversation");
        let reply = rig
            .post(&off_route, &body, &policy(true), Duration::ZERO)
            .await;
        assert_denied_without_dispatch(&rig, &reply, 404);
    }

    #[tokio::test]
    async fn a_target_that_is_not_a_seat_of_the_exposed_project_sends_nothing() {
        let rig = Rig::new();
        for (n, session) in [WORKER, "550e8400-e29b-41d4-a716-4466554400aa"]
            .into_iter()
            .enumerate()
        {
            let mut req = rig.request();
            req.path = format!("/api/v2/projects/{PROJECT}/managers/{session}/messages");
            let reply = rig
                .post(
                    &req,
                    &body_json(TEXT),
                    &policy(true),
                    Duration::from_secs(2 * n as u64),
                )
                .await;
            assert_eq!(reply.status, 403, "{session}");
        }
        assert_eq!(rig.fake.count("RemoteSendManagerMessageV2"), 0);
        assert!(rig.audit.lines().is_empty());

        // The daemon's answer is not a seat list: 502, still nothing sent.
        *lock(&rig.fake.seats) = Ok(b"not seats".to_vec());
        let reply = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(true),
                Duration::from_secs(10),
            )
            .await;
        assert_eq!(reply.status, 502);
        *lock(&rig.fake.seats) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(true),
                Duration::from_secs(20),
            )
            .await;
        assert_eq!((reply.status, reply.retry_after), (503, Some(1)));
        assert_eq!(rig.fake.count("RemoteSendManagerMessageV2"), 0);
        assert!(rig.audit.lines().is_empty());
    }

    #[tokio::test]
    async fn every_malformed_body_dispatches_nothing() {
        let rig = Rig::new();
        let base = String::from_utf8(body_json("a")).unwrap();
        let bodies: Vec<Vec<u8>> = vec![
            b"not json".to_vec(),
            b"[]".to_vec(),
            b"\"text\"".to_vec(),
            body_json(""),
            body_json("   \n"),
            body_json(&"x".repeat(rsi_common::remote_manager_v2::MAX_MESSAGE_BYTES + 1)),
            body_json("nul\u{0}byte"),
            base.replace(IDEM, "not-a-uuid").into_bytes(),
            base.replace(IDEM, &IDEM.to_uppercase()).into_bytes(),
            base.replace("\"text\"", "\"unknown\":1,\"text\"")
                .into_bytes(),
            br#"{"text":"hi"}"#.to_vec(),
            br#"{"text":7,"idempotency_key":"550e8400-e29b-41d4-a716-446655440009"}"#.to_vec(),
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
            assert_denied_without_dispatch(&rig, &reply, 400);
        }
        // A text of exactly the bound is accepted.
        let reply = rig
            .post(
                &rig.request(),
                &body_json(&"x".repeat(rsi_common::remote_manager_v2::MAX_MESSAGE_BYTES)),
                &policy(true),
                Duration::from_secs(100),
            )
            .await;
        assert_eq!(reply.status, 200);
    }

    #[tokio::test]
    async fn rate_limits_dispatch_nothing_and_say_when_to_retry() {
        let rig = Rig::new();
        assert_eq!(rig.send().await.status, 200);
        assert_eq!(rig.fake.sends().len(), 1);
        let reply = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(true),
                MIN_ANSWER_GAP / 3,
            )
            .await;
        assert_eq!(reply.status, 429);
        assert_eq!(reply.retry_after, Some(1));
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap(),
            json!({"error": "rate_limited"})
        );
        assert_eq!(
            rig.fake.sends().len(),
            1,
            "limited request was not forwarded"
        );
        assert_eq!(rig.audit.lines().len(), 2, "and wrote no audit line");
    }

    #[tokio::test]
    async fn a_timeout_after_the_write_is_unknown_and_the_same_key_retry_replays() {
        let rig = Rig::new();
        *lock(&rig.fake.send) = Err(OperatorError::Transport(ReadError::Timeout));
        let reply = rig.send().await;
        assert_eq!(reply.status, 504);
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap(),
            json!({"error": "outcome_unknown"})
        );
        let lines = rig.audit.lines();
        assert_eq!(lines[1]["result"], "unknown");
        assert_eq!(lines[1]["transport"], "timeout");
        assert_ne!(lines[1]["result"], "receipt");

        // The client retries with the same key; the daemon replays the queued
        // row instead of queuing a second message.
        *lock(&rig.fake.send) = Ok(receipt_body(true));
        let reply = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(true),
                Duration::from_secs(2),
            )
            .await;
        assert_eq!(reply.status, 200);
        let receipt: SendMessageReceiptV2 = serde_json::from_slice(&reply.body).unwrap();
        assert!(receipt.deduplicated);
        assert_eq!(rig.fake.sends().len(), 2);
        assert_eq!(
            rig.fake.sends()[0],
            rig.fake.sends()[1],
            "the identical request"
        );
        let lines = rig.audit.lines();
        assert_eq!(lines[2]["replay"], true);
        assert_eq!(lines[3]["deduplicated"], true);

        // Other text under the same key: the daemon refuses, 409, no second send.
        *lock(&rig.fake.send) = Err(OperatorError::Refused(
            "Policy denied: idempotency_conflict".into(),
        ));
        let reply = rig
            .post(
                &rig.request(),
                &body_json("something else"),
                &policy(true),
                Duration::from_secs(4),
            )
            .await;
        assert_eq!(reply.status, 409);
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap(),
            json!({"error": "idempotency_conflict"})
        );
        assert_eq!(
            rig.audit.lines().last().unwrap()["daemon_code"],
            "idempotency_conflict"
        );
    }

    #[tokio::test]
    async fn daemon_refusals_busy_and_invalid_receipts_map_to_their_statuses() {
        for (refusal, status) in [
            ("Policy denied: seat_out_of_scope", 403),
            ("Policy denied: seat_not_live", 409),
            ("Policy denied: invalid_message", 400),
            ("something unexpected", 403),
        ] {
            let rig = Rig::new();
            *lock(&rig.fake.send) = Err(OperatorError::Refused(refusal.into()));
            let reply = rig.send().await;
            assert_eq!(reply.status, status, "{refusal}");
            assert_eq!(rig.audit.lines()[1]["result"], "refused");
        }

        let rig = Rig::new();
        *lock(&rig.fake.send) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig.send().await;
        assert_eq!((reply.status, reply.retry_after), (503, Some(1)));
        assert_eq!(rig.audit.lines()[1]["result"], "not_sent");

        let rig = Rig::new();
        for (n, invalid) in [&b"{}"[..], br#"{"message_id":"x"}"#]
            .into_iter()
            .enumerate()
        {
            *lock(&rig.fake.send) = Ok(invalid.to_vec());
            let reply = rig
                .post(
                    &rig.request(),
                    &body_json(TEXT),
                    &policy(true),
                    Duration::from_secs(2 * (n as u64 + 1)),
                )
                .await;
            assert_eq!(reply.status, 502);
            let lines = rig.audit.lines();
            assert_eq!(lines.last().unwrap()["result"], "unknown");
            assert_eq!(lines.last().unwrap()["transport"], "malformed");
        }
    }

    #[tokio::test]
    async fn an_unwritable_audit_refuses_with_503_and_sends_nothing() {
        let rig = Rig::new();
        rig.audit.set_failing(true);
        let reply = rig.send().await;
        assert_eq!(reply.status, 503);
        assert_eq!(rig.fake.sends().len(), 0);
        assert!(rig.audit.lines().is_empty());
        assert!(AuditSink::append(&*rig.audit, &json!({})).is_err());
    }

    // ---- Contract fixtures (docs/rsi-remote/contracts-v2.md) ----

    /// One scripted exchange: the request a client sends and the gateway's
    /// reply, recorded as the contract fixture.
    struct Exchange {
        name: &'static str,
        request: Value,
        reply: AnswerReply,
    }

    fn request_record(method: &str, path: &str, csrf: bool, body: Option<Value>) -> Value {
        let mut headers = json!({
            "cookie": "__Host-rsi_remote=<session cookie>",
            "origin": "https://<canonical_host>",
        });
        if csrf {
            headers["x-rsi-csrf"] = json!("<csrf token>");
        }
        if body.is_some() {
            headers["content-type"] = json!("application/json");
        }
        json!({"method": method, "path": path, "headers": headers, "body": body})
    }

    fn reply_record(reply: &AnswerReply) -> Value {
        let body = if reply.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&reply.body).expect("a gateway body is JSON")
        };
        json!({"status": reply.status, "retry_after": reply.retry_after, "body": body})
    }

    fn get_record(path: &str) -> Value {
        request_record("GET", path, false, None)
    }

    fn message_path() -> String {
        format!("/api/v2/projects/{PROJECT}/managers/{PM_SEAT}/messages")
    }

    fn message_record(text: &str) -> Value {
        request_record(
            "POST",
            &message_path(),
            true,
            Some(json!({"text": text, "idempotency_key": IDEM})),
        )
    }

    async fn fixture_exchanges() -> Vec<Exchange> {
        let mut out = Vec::new();
        let list_path = "/api/v2/projects";
        let conversation_path =
            format!("/api/v2/projects/{PROJECT}/managers/{PM_SEAT}/conversation");
        let list = |rig: &Rig| rig.get(list_path, ingress::Query::default());

        let rig = Rig::new();
        let reply = rig.read(&list(&rig), &policy(false)).await;
        out.push(Exchange {
            name: "projects_with_global_200",
            request: get_record(list_path),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.seats) = Ok(seats_body(false));
        let reply = rig.read(&list(&rig), &policy(true)).await;
        out.push(Exchange {
            name: "projects_without_global_200",
            request: get_record(list_path),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.seats) = Ok(br#"{"projects":[]}"#.to_vec());
        let reply = rig.read(&list(&rig), &policy(false)).await;
        out.push(Exchange {
            name: "projects_empty_200",
            request: get_record(list_path),
            reply,
        });

        let rig = Rig::new();
        let reply = rig
            .read(
                &rig.get(&conversation_path, ingress::Query::default()),
                &policy(true),
            )
            .await;
        out.push(Exchange {
            name: "conversation_first_page_200",
            request: get_record(&conversation_path),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.conversation) = Ok(conversation_body(false));
        let older = ingress::Query {
            before: Some((3, 903)),
            ..ingress::Query::default()
        };
        let reply = rig
            .read(&rig.get(&conversation_path, older), &policy(true))
            .await;
        out.push(Exchange {
            name: "conversation_older_page_200",
            request: get_record(&format!("{conversation_path}?before=3:903")),
            reply,
        });

        let rig = Rig::new();
        let empty = ConversationResponseV2 {
            seat: seat(PM_SEAT, SeatRoleV2::ProjectManager),
            items: Vec::new(),
            next_before: None,
            has_more: false,
            operator_messages: Vec::new(),
        };
        *lock(&rig.fake.conversation) = Ok(serde_json::to_vec(&empty).unwrap());
        let reply = rig
            .read(
                &rig.get(&conversation_path, ingress::Query::default()),
                &policy(true),
            )
            .await;
        out.push(Exchange {
            name: "conversation_empty_200",
            request: get_record(&conversation_path),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.conversation) = Err(OperatorError::Refused("not_found".into()));
        let reply = rig
            .read(
                &rig.get(&conversation_path, ingress::Query::default()),
                &policy(true),
            )
            .await;
        out.push(Exchange {
            name: "conversation_unknown_seat_404",
            request: get_record(&conversation_path),
            reply,
        });

        let rig = Rig::new();
        let mut anonymous = list(&rig);
        anonymous.cookie = None;
        let reply = rig.read(&anonymous, &policy(true)).await;
        out.push(Exchange {
            name: "unauthenticated_401",
            request: get_record(list_path),
            reply,
        });

        let dashboard_path = "/api/v2/dashboard";
        for (name, body) in [
            ("dashboard_200", dashboard_body(true)),
            ("dashboard_empty_200", dashboard_body(false)),
            ("dashboard_incomplete_200", dashboard_incomplete_body()),
        ] {
            let rig = Rig::new();
            *lock(&rig.fake.dashboard) = Ok(body);
            let reply = rig
                .read(
                    &rig.get(dashboard_path, ingress::Query::default()),
                    &policy(false),
                )
                .await;
            out.push(Exchange {
                name,
                request: get_record(dashboard_path),
                reply,
            });
        }

        let rig = Rig::new();
        *lock(&rig.fake.dashboard) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig
            .read(
                &rig.get(dashboard_path, ingress::Query::default()),
                &policy(false),
            )
            .await;
        out.push(Exchange {
            name: "dashboard_busy_503",
            request: get_record(dashboard_path),
            reply,
        });

        let providers_path = "/api/v2/providers";
        for (name, body) in [
            ("providers_200", providers_body(true)),
            ("providers_empty_200", providers_body(false)),
        ] {
            let rig = Rig::new();
            *lock(&rig.fake.providers) = Ok(body);
            let reply = rig
                .read(
                    &rig.get(providers_path, ingress::Query::default()),
                    &policy(false),
                )
                .await;
            out.push(Exchange {
                name,
                request: get_record(providers_path),
                reply,
            });
        }

        let rig = Rig::new();
        *lock(&rig.fake.providers) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig
            .read(
                &rig.get(providers_path, ingress::Query::default()),
                &policy(false),
            )
            .await;
        out.push(Exchange {
            name: "providers_busy_503",
            request: get_record(providers_path),
            reply,
        });

        let rig = Rig::new();
        let reply = rig.send().await;
        out.push(Exchange {
            name: "message_accepted_200",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.send) = Ok(receipt_body(true));
        let reply = rig.send().await;
        out.push(Exchange {
            name: "message_replayed_200",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        let reply = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(false),
                Duration::ZERO,
            )
            .await;
        out.push(Exchange {
            name: "message_switch_off_403",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        let mut bad_csrf = rig.request();
        bad_csrf.csrf = Some("f".repeat(64));
        let reply = rig
            .post(&bad_csrf, &body_json(TEXT), &policy(true), Duration::ZERO)
            .await;
        out.push(Exchange {
            name: "message_bad_csrf_403",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        let mut elsewhere = rig.request();
        elsewhere.path = format!("/api/v2/projects/{PROJECT}/managers/{WORKER}/messages");
        let reply = rig
            .post(&elsewhere, &body_json(TEXT), &policy(true), Duration::ZERO)
            .await;
        out.push(Exchange {
            name: "message_not_a_seat_403",
            request: request_record(
                "POST",
                &format!("/api/v2/projects/{PROJECT}/managers/{WORKER}/messages"),
                true,
                Some(json!({"text": TEXT, "idempotency_key": IDEM})),
            ),
            reply,
        });

        let rig = Rig::new();
        let reply = rig
            .post(
                &rig.request(),
                br#"{"text":"","idempotency_key":"550e8400-e29b-41d4-a716-446655440009"}"#,
                &policy(true),
                Duration::ZERO,
            )
            .await;
        out.push(Exchange {
            name: "message_invalid_400",
            request: message_record(""),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.send) = Err(OperatorError::Refused(
            "Policy denied: idempotency_conflict".into(),
        ));
        let reply = rig.send().await;
        out.push(Exchange {
            name: "message_key_conflict_409",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        rig.send().await;
        let reply = rig
            .post(
                &rig.request(),
                &body_json(TEXT),
                &policy(true),
                MIN_ANSWER_GAP / 3,
            )
            .await;
        out.push(Exchange {
            name: "message_rate_limited_429",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.send) = Err(OperatorError::Transport(ReadError::Timeout));
        let reply = rig.send().await;
        out.push(Exchange {
            name: "message_outcome_unknown_504",
            request: message_record(TEXT),
            reply,
        });

        let rig = Rig::new();
        *lock(&rig.fake.send) = Err(OperatorError::Transport(ReadError::Busy));
        let reply = rig.send().await;
        out.push(Exchange {
            name: "message_busy_503",
            request: message_record(TEXT),
            reply,
        });
        out
    }

    fn fixtures_dir() -> std::path::PathBuf {
        std::path::Path::new(rsi_common::manifest_dir!()).join("../../remote/contracts/v2/fixtures")
    }

    /// The checked-in fixtures are exactly what the gateway produces. Run with
    /// `RSI_REGEN_CONTRACT_FIXTURES=1` to rewrite them after an intended change.
    #[tokio::test]
    async fn contract_fixtures_are_produced_by_the_code() {
        let dir = fixtures_dir();
        let exchanges = fixture_exchanges().await;
        let regenerate = std::env::var_os("RSI_REGEN_CONTRACT_FIXTURES").is_some();
        if regenerate {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let mut expected = std::collections::BTreeSet::new();
        for exchange in &exchanges {
            let record = json!({
                "name": exchange.name,
                "request": exchange.request,
                "response": reply_record(&exchange.reply),
            });
            let path = dir.join(format!("{}.json", exchange.name));
            expected.insert(format!("{}.json", exchange.name));
            if regenerate {
                std::fs::write(&path, serde_json::to_string_pretty(&record).unwrap() + "\n")
                    .unwrap();
            }
            let on_disk: Value = serde_json::from_slice(
                &std::fs::read(&path)
                    .unwrap_or_else(|_| panic!("missing fixture {}", path.display())),
            )
            .unwrap();
            assert_eq!(
                on_disk, record,
                "fixture {} drifted from the code",
                exchange.name
            );
        }
        let listed: std::collections::BTreeSet<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(listed, expected, "a fixture exists that no test produces");
    }

    /// Every success body in the fixtures is the canonical serialization of
    /// the wire type, so the type and the file cannot disagree.
    #[tokio::test]
    async fn success_fixtures_round_trip_through_the_wire_types() {
        for exchange in fixture_exchanges().await {
            if exchange.reply.status != 200 {
                continue;
            }
            let body: Value = serde_json::from_slice(&exchange.reply.body).unwrap();
            let again = if exchange.name.starts_with("projects_") {
                serde_json::to_value(
                    serde_json::from_value::<ProjectsResponseV2>(body.clone()).unwrap(),
                )
            } else if exchange.name.starts_with("dashboard_") {
                serde_json::to_value(
                    serde_json::from_value::<DashboardResponseV2>(body.clone()).unwrap(),
                )
            } else if exchange.name.starts_with("providers_") {
                serde_json::to_value(
                    serde_json::from_value::<ProvidersResponseV2>(body.clone()).unwrap(),
                )
            } else if exchange.name.starts_with("conversation_") {
                serde_json::to_value(
                    serde_json::from_value::<ConversationResponseV2>(body.clone()).unwrap(),
                )
            } else {
                serde_json::to_value(
                    serde_json::from_value::<SendMessageReceiptV2>(body.clone()).unwrap(),
                )
            }
            .unwrap();
            assert_eq!(again, body, "{}", exchange.name);
        }
    }
}
