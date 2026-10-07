//! #1049: claim pending mail for a RUNNING session at a tool boundary.
//!
//! A Claude CLI turn is one process, so mail for a busy Claude session used to
//! wait for the turn to end (#274) and an operator message killed the in-flight
//! tool call. rsi now installs a `PostToolUse` hook for the sessions it
//! launches; after each tool call the hook asks the daemon (the
//! `ClaimBoundaryMail` verb, authenticated by the session's own token) for the
//! messages that are pending for this session, and Claude shows them to the
//! model as `additionalContext`. The tool call is never cancelled.
//!
//! This module is the daemon half: ONE entry point,
//! [`claim_boundary_mail`], that turns "pending boundary messages for this
//! session" into an ordered list of labelled, already-claimed messages.
//!
//! # Delivery semantics (one message, one claim)
//!
//! Each message goes through the same durable sequence the idle-boundary and
//! Harness deliveries use: arbiter grant -> `claim_agent_message_exact`
//! (`queued -> claimed`, one attempt row) -> `dispatching` marker -> the text is
//! handed to the hook. A message is therefore returned at most once: after the
//! claim it is no longer `queued`, so the next hook (or the turn-end
//! dispatcher) cannot pick it up again.
//!
//! # Delivery-correlated acknowledgement (#1183)
//!
//! Handing the text to the hook is not delivery, and neither is the hook
//! printing it: the hook can die first, and the provider can time the hook out
//! and discard its output. The attempt therefore stays `dispatching` (the
//! message `claimed`) through three steps: the hook writes and flushes its
//! output, then calls `ConfirmBoundaryMail` ([`confirm_boundary_mail`]) with
//! the provider's own transcript locator from its hook input; the daemon then
//! watches that transcript ([`await_boundary_mail_evidence`]) for the
//! provider's record that it ACCEPTED the hook context (Codex: a developer
//! `hooks.additional_context` item; Claude: a `hook_additional_context`
//! attachment) containing the message's exact block. Only then is
//! `AdmittedEffectPossible` recorded (`injected`), so only mail the model was
//! actually given can be acknowledged by a later assistant event. Anything
//! else (reply not written, hook died, provider discarded the output, an
//! unverifiable locator, or a record that could not be persisted) stays
//! pending until [`BOUNDARY_MAIL_CONFIRM_DEADLINE`] and settles `uncertain`
//! ([`settle_unconfirmed_boundary_mail`]): visible, never re-sent, never
//! acknowledged. A daemon death in between leaves it `dispatching`, which
//! startup reconciliation classifies `uncertain` (#945).
//!
//! The delivery invocation is the session's CURRENT model invocation (the
//! running turn's own): a boundary hook is not a new model call, so nothing new
//! is admitted or billed. A session with no current invocation claims nothing.
//!
//! # The operator queue (#929)
//!
//! [`claim_boundary_mail`] is the only function the RPC handler calls. The
//! operator's queued messages (#929) are a second SOURCE of
//! [`BoundaryMail`], claimed by [`claim_operator_queue`] with
//! `sender_role: "operator"`. The row stays `effect_possible` until the RPC reply
//! carrying its text has been written to the hook (#1062); then it is settled
//! `delivered` in the same transaction that writes an operator-labelled user
//! event into the session transcript (through the session's monitor, which owns
//! the sequence counter). A daemon death before the reply is written leaves it
//! `uncertain`, never `delivered`. Operator messages go first, then agent mail.
//! Anything still queued at turn end is delivered by the turn-end dispatcher as
//! before.

use std::collections::HashMap;
use std::sync::Arc;

use rsi_common::agent_coordination::{
    BoundaryAdmissionV1, BoundaryCapabilityKindV1, BoundaryClassificationV1,
    BoundaryProviderKindV1, MessageAttemptFenceV1,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::error::Result;
use crate::session::agent_message_arbiter::{
    AgentMessageArbiter, ArbitrationGrant, BoundaryDecision, decide_next_boundary,
};
use crate::session::agent_message_delivery::build_claim_request;
use crate::session::types::TrackedSession;
use crate::store::Store;
use crate::store::agent_coordination::{ClaimAgentMessageOutcome, NoEffectDisposition};

/// At most this many messages are handed over per tool boundary; the rest wait
/// for the next boundary (a hook must stay small and fast).
pub(crate) const BOUNDARY_MAIL_MAX_PER_CLAIM: usize = 4;

/// How long a hook has to confirm it printed the mail it claimed (#1183). The
/// hook itself is killed by its provider after a few seconds, so a delivery
/// still unconfirmed after this is settled `uncertain`.
pub(crate) const BOUNDARY_MAIL_CONFIRM_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Where a hook said the provider records what it accepted (#1183).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptEvidence {
    path: std::path::PathBuf,
    /// Bytes already scanned. Starts at the file length when the hook
    /// confirmed: the provider records accepted context only after the hook
    /// exits, so nothing earlier can be the evidence.
    cursor: u64,
}

/// One agent message handed to a hook and not yet delivered.
#[derive(Debug, Clone)]
struct PendingBoundaryDelivery {
    session_id: Uuid,
    fence: MessageAttemptFenceV1,
    authority_id: Uuid,
    provider_kind: BoundaryProviderKindV1,
    /// The exact block the hook prints, looked for in the provider transcript.
    block: String,
    /// Set once the hook confirmed it printed the block.
    evidence: Option<TranscriptEvidence>,
}

/// Daemon-wide ledger of hook deliveries awaiting proof of delivery (#1183).
/// Each entry is taken exactly once: by the evidence watcher once the
/// provider transcript shows the hook context was accepted (and the delivery
/// is persisted), or by the settlement that marks it `uncertain`.
#[derive(Debug, Default)]
pub(crate) struct PendingBoundaryDeliveries {
    entries: std::sync::Mutex<HashMap<Uuid, PendingBoundaryDelivery>>,
    #[cfg(test)]
    fail_next_record: std::sync::atomic::AtomicBool,
}

impl PendingBoundaryDeliveries {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, PendingBoundaryDelivery>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn insert(&self, delivery: PendingBoundaryDelivery) {
        self.lock().insert(delivery.fence.message_id, delivery);
    }

    /// Record that `session_id`'s hook printed `message_id`. Only the claiming
    /// session's own, not-yet-printed delivery qualifies.
    fn mark_printed(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        evidence: TranscriptEvidence,
    ) -> bool {
        let mut entries = self.lock();
        match entries.get_mut(&message_id) {
            Some(delivery) if delivery.session_id == session_id && delivery.evidence.is_none() => {
                delivery.evidence = Some(evidence);
                true
            }
            _ => false,
        }
    }

    fn get(&self, message_id: Uuid) -> Option<PendingBoundaryDelivery> {
        self.lock().get(&message_id).cloned()
    }

    fn advance_cursor(&self, message_id: Uuid, cursor: u64) {
        if let Some(evidence) = self
            .lock()
            .get_mut(&message_id)
            .and_then(|delivery| delivery.evidence.as_mut())
        {
            evidence.cursor = cursor;
        }
    }

    fn take(&self, message_id: Uuid) -> Option<PendingBoundaryDelivery> {
        self.lock().remove(&message_id)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    #[cfg(test)]
    fn fail_next_record(&self) {
        self.fail_next_record
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test seam: make the next delivery record fail like a store error.
    fn take_injected_record_failure(&self) -> bool {
        #[cfg(test)]
        {
            self.fail_next_record
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

/// How often the evidence watcher re-reads the provider transcript.
pub(crate) const BOUNDARY_MAIL_EVIDENCE_POLL: std::time::Duration =
    std::time::Duration::from_millis(100);

/// Largest transcript tail one poll reads.
const TRANSCRIPT_SCAN_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// The reply to `ConfirmBoundaryMail`: the ids now awaiting transcript
/// evidence. Printing is not delivery; nothing is `injected` yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmBoundaryMailResponse {
    pub confirmed: Vec<Uuid>,
}

/// One claimed message, ready to show the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryMail {
    pub message_id: Uuid,
    /// `operator`, or the sending session's role (`manager`, `lead`, ...).
    pub sender_role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_session_id: Option<Uuid>,
    /// The provider-ready, attributed envelope text.
    pub text: String,
}

/// The reply to `ClaimBoundaryMail`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimBoundaryMailResponse {
    pub messages: Vec<BoundaryMail>,
}

fn sender_role_label(store: &Store, owner_session_id: Uuid) -> String {
    match store.get_session(owner_session_id) {
        Ok(Some(owner)) => owner
            .agent_role
            .filter(|role| !role.trim().is_empty())
            .unwrap_or_else(|| format!("{:?}", owner.session_kind).to_lowercase()),
        _ => "agent".to_string(),
    }
}

/// Claim the messages pending for `session_id` at this tool boundary.
///
/// Never blocks on the model and never holds the store guard across an
/// `.await` that leaves the daemon. A refused or failed claim leaves the
/// message queued (it is delivered at the next boundary or as the next turn).
pub(crate) async fn claim_boundary_mail(
    store: &Arc<Mutex<Store>>,
    arbiter: &Arc<AgentMessageArbiter>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    pending: &PendingBoundaryDeliveries,
    session_id: Uuid,
) -> Result<ClaimBoundaryMailResponse> {
    let mut response = ClaimBoundaryMailResponse::default();
    let Some(generation) = ({
        let guard = active.read().await;
        guard
            .get(&session_id)
            .map(|tracked| tracked.spawn_generation)
    }) else {
        return Ok(response);
    };

    // Queued OPERATOR messages (#929) go first, then agent mail.
    claim_operator_queue(store, session_id, &mut response.messages).await;

    while response.messages.len() < BOUNDARY_MAIL_MAX_PER_CLAIM {
        let grant = {
            let guard = store.lock().await;
            match decide_next_boundary(&guard, arbiter, session_id, generation)? {
                BoundaryDecision::DeliverMail(grant) => grant,
                BoundaryDecision::SyntheticContinuation(_) => break,
            }
        };
        match claim_one(store, pending, session_id, grant).await? {
            Some(mail) => response.messages.push(mail),
            None => break,
        }
    }
    Ok(response)
}

/// Claim the session's next queued operator message (#929) for this tool
/// boundary.
///
/// Same durable sequence the turn-end dispatcher uses: `queued -> dispatching`
/// (the store's single-claim transaction: one open row per session, so a
/// message is claimed at most once across the hook and the turn-end dispatcher)
/// -> `effect_possible`. The row is NOT settled here (#1062): it stays
/// `effect_possible` until the RPC reply carrying its text has been written to
/// the hook, and only [`finalize_operator_boundary_delivery`] settles it
/// `delivered` (atomically with its transcript event). A daemon death before
/// that leaves the row `effect_possible`, which startup reconciliation marks
/// `uncertain` (#945): visible, never re-sent, never reported delivered. A row
/// that cannot cross the effect boundary is put back to `queued`. A store error
/// stops the claim quietly; the message stays queued for the next boundary or
/// the turn-end dispatch. One operator message per claim: the open row blocks a
/// second claim until it settles, and the next boundary picks the next one up.
async fn claim_operator_queue(
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    out: &mut Vec<BoundaryMail>,
) {
    let mut guard = store.lock().await;
    let message = match guard.claim_operator_message(session_id) {
        Ok(Some(message)) => message,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(session_id = %session_id, error = %error, "operator boundary claim failed");
            return;
        }
    };
    if let Err(error) = guard.mark_operator_message_effect_possible(message.id) {
        tracing::warn!(message_id = %message.id, error = %error, "operator boundary message could not cross the effect boundary");
        if let Err(settle) = guard.settle_operator_message(message.id, false) {
            tracing::warn!(message_id = %message.id, error = %settle, "operator boundary message could not be released");
        }
        return;
    }
    drop(guard);
    out.push(BoundaryMail {
        message_id: message.id,
        sender_role: OPERATOR_SENDER_ROLE.to_string(),
        sender_session_id: None,
        text: message.content,
    });
}

/// `sender_role` of a boundary message that came from the operator's queue.
pub(crate) const OPERATOR_SENDER_ROLE: &str = "operator";

/// The operator-delivery ids in a rendered `ClaimBoundaryMail` reply.
#[must_use]
pub(crate) fn operator_message_ids(result: &serde_json::Value) -> Vec<Uuid> {
    result
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| {
            message
                .get("sender_role")
                .and_then(serde_json::Value::as_str)
                == Some(OPERATOR_SENDER_ROLE)
        })
        .filter_map(|message| message.get("message_id")?.as_str()?.parse().ok())
        .collect()
}

/// Per-session hand-off from the RPC layer to the session's monitor (#1062).
///
/// The monitor owns the session's transcript sequence counter, so a boundary
/// delivery is never written by a second, racing writer: the RPC layer queues
/// the message id here and the monitor, which alone allocates sequences, writes
/// the event and settles the row.
#[derive(Default)]
pub(crate) struct OperatorTranscriptInbox {
    pending: std::sync::Mutex<Vec<Uuid>>,
    wake: tokio::sync::Notify,
}

impl OperatorTranscriptInbox {
    pub(crate) fn push(&self, message_id: Uuid) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message_id);
        self.wake.notify_one();
    }

    pub(crate) fn take(&self) -> Vec<Uuid> {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub(crate) async fn notified(&self) {
        self.wake.notified().await;
    }
}

/// The reply carrying `message_ids` reached the hook. Hand each to the session's
/// live monitor to write its transcript event and settle `delivered`; with no
/// live monitor there is no racing writer, so write and settle here.
pub(crate) async fn finalize_operator_boundary_delivery(
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: &Arc<crate::bus::EventBus>,
    message_ids: &[Uuid],
) {
    for &message_id in message_ids {
        let session_id = match store.lock().await.operator_message_session(message_id) {
            Ok(Some(session_id)) => session_id,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%message_id, %error, "operator boundary delivery could not be located");
                continue;
            }
        };
        let inbox = active
            .read()
            .await
            .get(&session_id)
            .map(|tracked| tracked.operator_inbox.clone());
        if let Some(inbox) = inbox {
            inbox.push(message_id);
            continue;
        }
        let delivered = store
            .lock()
            .await
            .deliver_operator_message_with_transcript(message_id, None);
        match delivered {
            Ok(event) => {
                event_bus.publish(crate::bus::DaemonEvent::ConversationEvent { session_id, event })
            }
            Err(error) => {
                tracing::warn!(%message_id, %error, "operator boundary delivery could not be recorded; the row stays uncertain");
            }
        }
    }
}

/// The reply could not be written: the hook may or may not have seen the text,
/// so the rows become `uncertain` (#945) rather than staying open forever.
pub(crate) async fn abandon_operator_boundary_delivery(
    store: &Arc<Mutex<Store>>,
    message_ids: &[Uuid],
) {
    for &message_id in message_ids {
        if let Err(error) = store
            .lock()
            .await
            .settle_operator_message(message_id, false)
        {
            tracing::warn!(%message_id, %error, "operator boundary delivery could not be marked uncertain");
        }
    }
}

/// Monitor side: write each queued boundary delivery into the transcript with
/// the monitor's own sequence counter, then publish it. A row that cannot be
/// recorded keeps its sequence number free and stays `effect_possible`.
pub(crate) async fn drain_operator_transcript_inbox(
    inbox: &OperatorTranscriptInbox,
    store: &Arc<Mutex<Store>>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: &Arc<crate::bus::EventBus>,
    session_id: Uuid,
    sequence: &mut i32,
) {
    for message_id in inbox.take() {
        let Some(next) = sequence.checked_add(1) else {
            tracing::error!(%session_id, "transcript sequence exhausted; operator delivery left uncertain");
            return;
        };
        let delivered = store
            .lock()
            .await
            .deliver_operator_message_with_transcript(message_id, Some(next));
        match delivered {
            Ok(event) => {
                *sequence = next;
                if let Some(tracked) = active.write().await.get_mut(&session_id) {
                    tracked.events.push(event.clone());
                }
                event_bus.publish(crate::bus::DaemonEvent::ConversationEvent { session_id, event });
            }
            Err(error) => {
                tracing::warn!(%message_id, %error, "operator boundary delivery could not be recorded; the row stays uncertain");
            }
        }
    }
}

/// The agent-mail (non-operator) delivery ids in a rendered
/// `ClaimBoundaryMail` reply.
#[must_use]
pub(crate) fn agent_message_ids(result: &serde_json::Value) -> Vec<Uuid> {
    result
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| {
            message
                .get("sender_role")
                .and_then(serde_json::Value::as_str)
                != Some(OPERATOR_SENDER_ROLE)
        })
        .filter_map(|message| message.get("message_id")?.as_str()?.parse().ok())
        .collect()
}

/// `ConfirmBoundaryMail` (#1183): `session_id`'s hook wrote and flushed its
/// output for `params.message_ids`. That only proves the hook printed: the
/// provider may still time the hook out and discard it. So nothing is
/// recorded here; each still-pending delivery of this session's hook is
/// marked printed with the provider transcript to watch, and
/// [`await_boundary_mail_evidence`] records it `injected` only once that
/// transcript shows the provider accepted the hook context.
///
/// # Errors
///
/// `confirm_boundary_mail_unverifiable_transcript` when the hook's transcript
/// locator is missing or does not belong to this session's provider
/// conversation. The deliveries stay pending, so the deadline settles them
/// `uncertain`.
pub(crate) async fn confirm_boundary_mail(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    pending: &PendingBoundaryDeliveries,
    session_id: Uuid,
    params: &rsi_common::boundary_mail_hook::ConfirmBoundaryMailParams,
) -> Result<ConfirmBoundaryMailResponse> {
    let provider_session_id = active
        .read()
        .await
        .get(&session_id)
        .and_then(|tracked| tracked.session.claude_session_id.clone());
    let path = verified_transcript_path(
        provider_session_id.as_deref(),
        params.provider_session_id.as_deref(),
        params.transcript_path.as_deref(),
    )
    .ok_or_else(|| {
        crate::error::DaemonError::InvalidParam(
            "confirm_boundary_mail_unverifiable_transcript".into(),
        )
    })?;
    let cursor = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let mut response = ConfirmBoundaryMailResponse::default();
    for &message_id in &params.message_ids {
        let evidence = TranscriptEvidence {
            path: path.clone(),
            cursor,
        };
        if pending.mark_printed(session_id, message_id, evidence) {
            response.confirmed.push(message_id);
        }
    }
    Ok(response)
}

/// The hook's transcript path, accepted only when it is this session's own
/// provider conversation: the hook's provider session id equals the one rsid
/// recorded for the session, and the path is an absolute `.jsonl` file named
/// for it (`<id>.jsonl` for Claude, `rollout-...-<id>.jsonl` for Codex).
fn verified_transcript_path(
    recorded: Option<&str>,
    claimed: Option<&str>,
    path: Option<&str>,
) -> Option<std::path::PathBuf> {
    let recorded = recorded.filter(|id| !id.trim().is_empty())?;
    if claimed? != recorded {
        return None;
    }
    let path = std::path::PathBuf::from(path?);
    let name = path.file_name()?.to_str()?;
    let named_for_session =
        name == format!("{recorded}.jsonl") || name.ends_with(&format!("-{recorded}.jsonl"));
    (path.is_absolute() && named_for_session && path.is_file()).then_some(path)
}

/// Read the complete transcript lines after `cursor`; returns the lines and
/// the cursor past the last complete one.
fn read_transcript_tail(path: &std::path::Path, cursor: u64) -> Option<(Vec<String>, u64)> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = cursor.min(len);
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TRANSCRIPT_SCAN_MAX_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let lines = String::from_utf8_lossy(&bytes[..complete])
        .lines()
        .map(str::to_string)
        .collect();
    Some((lines, start + complete as u64))
}

/// Watch the provider transcript for each printed delivery in
/// `message_ids` until `deadline` (#1183). A delivery whose block appears as
/// accepted hook context is recorded `AdmittedEffectPossible` (`injected`),
/// and only then can a later assistant event acknowledge it. If that record
/// cannot be persisted the delivery stays pending, so the deadline settles it
/// `uncertain`. Deliveries with no evidence are left for that settlement.
pub(crate) async fn await_boundary_mail_evidence(
    store: &Arc<Mutex<Store>>,
    pending: &PendingBoundaryDeliveries,
    message_ids: &[Uuid],
    deadline: tokio::time::Instant,
    poll: std::time::Duration,
) {
    let mut open: Vec<Uuid> = message_ids.to_vec();
    loop {
        let mut still_open = Vec::new();
        for message_id in open {
            let Some(delivery) = pending.get(message_id) else {
                continue;
            };
            let Some(evidence) = delivery.evidence.clone() else {
                continue;
            };
            let Some((lines, cursor)) = read_transcript_tail(&evidence.path, evidence.cursor)
            else {
                still_open.push(message_id);
                continue;
            };
            let accepted = lines.iter().any(|line| {
                rsi_common::boundary_mail_hook::transcript_line_accepts_block(line, &delivery.block)
            });
            if !accepted {
                pending.advance_cursor(message_id, cursor);
                still_open.push(message_id);
                continue;
            }
            let Some(delivery) = pending.take(message_id) else {
                continue;
            };
            let recorded = if pending.take_injected_record_failure() {
                Err(crate::error::DaemonError::Store(
                    "injected boundary delivery record failure".into(),
                ))
            } else {
                record(
                    store,
                    &delivery.fence,
                    delivery.authority_id,
                    BoundaryClassificationV1::AdmittedEffectPossible,
                    None,
                    delivery.provider_kind,
                )
                .await
            };
            if let Err(error) = recorded {
                tracing::warn!(
                    target: "agent_coordination",
                    %message_id,
                    %error,
                    "boundary mail delivery could not be recorded; it stays pending and \
                     the deadline settles it uncertain"
                );
                pending.insert(delivery);
                still_open.push(message_id);
            }
        }
        open = still_open;
        if open.is_empty() || tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Settle `message_ids` that were handed to a hook but not confirmed
/// (#1183): each still-pending one becomes `uncertain`. Called when the reply
/// could not be written and when [`BOUNDARY_MAIL_CONFIRM_DEADLINE`] passes.
pub(crate) async fn settle_unconfirmed_boundary_mail(
    store: &Arc<Mutex<Store>>,
    pending: &PendingBoundaryDeliveries,
    message_ids: &[Uuid],
) {
    for &message_id in message_ids {
        let Some(delivery) = pending.take(message_id) else {
            continue;
        };
        let settled = store
            .lock()
            .await
            .mark_unconfirmed_boundary_delivery_uncertain_v1(&delivery.fence, Uuid::new_v4());
        match settled {
            Ok(true) => tracing::info!(
                target: "agent_coordination",
                %message_id,
                "boundary mail was not confirmed by the hook; settled uncertain"
            ),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                target: "agent_coordination",
                %message_id,
                %error,
                "unconfirmed boundary mail could not be settled; it stays dispatching \
                 and restart reconciliation classifies it uncertain"
            ),
        }
    }
}

async fn claim_one(
    store: &Arc<Mutex<Store>>,
    pending: &PendingBoundaryDeliveries,
    session_id: Uuid,
    grant: Box<ArbitrationGrant>,
) -> Result<Option<BoundaryMail>> {
    let Some(invocation_id) = grant.request().expected_prior_model_invocation_id else {
        // No running turn invocation to attach the attempt to.
        grant.release();
        return Ok(None);
    };
    let claim = {
        let guard = store.lock().await;
        build_claim_request(&grant, invocation_id, guard.delivery_boot_id())
    };
    let authority_id = claim.authority_id;
    let fence = {
        let guard = store.lock().await;
        match guard.claim_agent_message_exact(&claim) {
            Ok(ClaimAgentMessageOutcome::Claimed(fence)) => fence,
            Ok(ClaimAgentMessageOutcome::CasLost(_)) => {
                grant.release();
                return Ok(None);
            }
            Err(error) => {
                grant.release();
                return Err(error);
            }
        }
    };

    let marked = {
        let guard = store.lock().await;
        guard.mark_agent_message_attempt_dispatching(&fence)
    };
    if let Err(error) = marked {
        tracing::warn!(
            target: "agent_coordination",
            message_id = %fence.message_id,
            error = %error,
            "boundary mail dispatch marker failed; refusing to hand the message over"
        );
        record(
            store,
            &fence,
            authority_id,
            BoundaryClassificationV1::RejectedBeforeEffect,
            Some("agent_message_dispatch_marker_failed"),
            grant.request().provider_kind,
        )
        .await
        .ok();
        grant.release();
        return Ok(None);
    }

    let (sender_role, text) = {
        let guard = store.lock().await;
        (
            sender_role_label(&guard, grant.request().owner_session_id),
            grant.render_payload_for_delivery(),
        )
    };
    let sender_session_id = Some(grant.request().owner_session_id);
    let message_id = fence.message_id;
    // NOT recorded here (#1183): the attempt stays `dispatching` until the hook
    // confirms it printed the text. The claim already makes it at most once.
    pending.insert(PendingBoundaryDelivery {
        session_id,
        fence,
        authority_id,
        provider_kind: grant.request().provider_kind,
        block: rsi_common::boundary_mail_hook::render_mail_block(&sender_role, &text),
        evidence: None,
    });
    grant.release();
    Ok(Some(BoundaryMail {
        message_id,
        sender_role,
        sender_session_id,
        text,
    }))
}

async fn record(
    store: &Arc<Mutex<Store>>,
    fence: &MessageAttemptFenceV1,
    authority_id: Uuid,
    classification: BoundaryClassificationV1,
    error_class: Option<&'static str>,
    provider_kind: BoundaryProviderKindV1,
) -> Result<()> {
    let admission = BoundaryAdmissionV1 {
        provider_kind,
        capability_kind: provider_kind.capability_kind(),
        delivery_session_id: fence.delivery_session_id,
        session_generation: fence.delivery_session_generation,
        model_invocation_id: fence.delivery_model_invocation_id,
        native_turn_id: None,
        classification,
        provider_error_class: error_class.map(str::to_string),
    };
    debug_assert!(matches!(
        admission.capability_kind,
        BoundaryCapabilityKindV1::TerminalOneTurn | BoundaryCapabilityKindV1::HarnessToolBoundary
    ));
    let outcome = {
        let guard = store.lock().await;
        guard.record_agent_message_admission(
            fence,
            &admission,
            NoEffectDisposition::Requeue,
            authority_id,
        )
    };
    if let Err(error) = &outcome {
        tracing::warn!(
            target: "agent_coordination",
            message_id = %fence.message_id,
            error = %error,
            "failed to record a boundary mail delivery; the attempt stays claimed/dispatching"
        );
    }
    outcome.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_message_delivery::tests::tracked;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::agent_coordination::AgentSendMessageRequestV1;
    use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};

    struct Fixture {
        store: Arc<Mutex<Store>>,
        arbiter: Arc<AgentMessageArbiter>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        pending: PendingBoundaryDeliveries,
        owner: Uuid,
        target: Uuid,
        /// The provider transcript of the target (#1183 evidence).
        transcript: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    /// The provider conversation id rsid recorded for the target.
    const THREAD: &str = "thread-1183";

    fn message_state(store: &Store, message_id: Uuid) -> String {
        store
            .conn
            .query_row(
                "SELECT state FROM agent_messages WHERE id=?1",
                rusqlite::params![message_id.to_string()],
                |row| row.get(0),
            )
            .expect("message state")
    }

    /// A running Claude session (with a current turn invocation, unless
    /// `with_invocation` is false) and a manager-role sender.
    fn fixture(with_invocation: bool) -> Fixture {
        let store = Store::open_in_memory().expect("store");
        store
            .set_delivery_boot_id(Uuid::new_v4())
            .expect("seed delivery boot id");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        let mut owner_row = test_session(owner, std::path::PathBuf::from("/tmp"));
        owner_row.session_kind = SessionKind::Task;
        owner_row.status = SessionStatus::Running;
        owner_row.agent_role = Some("manager".to_string());
        store.insert_session(&owner_row).expect("owner");
        let mut row = test_session(target, std::path::PathBuf::from("/tmp"));
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.provider = SessionProvider::Claude;
        store.insert_session(&row).expect("target");
        if with_invocation {
            let invocation = Uuid::new_v4();
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations (
                         id, purpose, invocation_kind, foreground, paid_risk,
                         admission_status, status, trigger_source, session_id,
                         policy_snapshot_json, created_at, started_at
                     ) VALUES (?1, 'session.claude.turn', 'model', 'foreground',
                         'paid_capable', 'admitted', 'running', 'boundary_mail_tests',
                         ?2, '{}', ?3, ?3)",
                    rusqlite::params![invocation.to_string(), target.to_string(), now],
                )
                .expect("invocation");
            store
                .conn
                .execute(
                    "UPDATE sessions SET model_invocation_id=?1 WHERE id=?2",
                    rusqlite::params![invocation.to_string(), target.to_string()],
                )
                .expect("bind invocation");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir
            .path()
            .join(format!("rollout-2026-10-05-{THREAD}.jsonl"));
        std::fs::write(&transcript, "{\"type\":\"session_meta\"}\n").expect("transcript");
        let mut map = HashMap::new();
        let mut tracked_row = tracked(row);
        tracked_row.session.claude_session_id = Some(THREAD.to_string());
        map.insert(target, tracked_row);
        Fixture {
            store: Arc::new(Mutex::new(store)),
            arbiter: Arc::new(AgentMessageArbiter::new()),
            active: Arc::new(RwLock::new(map)),
            pending: PendingBoundaryDeliveries::default(),
            owner,
            target,
            transcript,
            _dir: dir,
        }
    }

    async fn send(fixture: &Fixture, key: &str, body: &str) -> Uuid {
        let store = fixture.store.lock().await;
        store
            .accept_agent_message(
                fixture.owner,
                None,
                &AgentSendMessageRequestV1 {
                    target_session_id: fixture.target,
                    message: body.to_string(),
                    idempotency_key: key.to_string(),
                    expires_at: None,
                },
            )
            .expect("accept")
            .receipt()
            .message_id
    }

    async fn claim(fixture: &Fixture) -> ClaimBoundaryMailResponse {
        claim_boundary_mail(
            &fixture.store,
            &fixture.arbiter,
            &fixture.active,
            &fixture.pending,
            fixture.target,
        )
        .await
        .expect("claim")
    }

    fn confirm_params(
        fixture: &Fixture,
        ids: &[Uuid],
    ) -> rsi_common::boundary_mail_hook::ConfirmBoundaryMailParams {
        rsi_common::boundary_mail_hook::ConfirmBoundaryMailParams {
            message_ids: ids.to_vec(),
            provider_session_id: Some(THREAD.to_string()),
            transcript_path: Some(fixture.transcript.display().to_string()),
        }
    }

    /// The hook printed `ids` (ConfirmBoundaryMail with its real locator).
    async fn confirm(fixture: &Fixture, session_id: Uuid, ids: &[Uuid]) -> Vec<Uuid> {
        confirm_boundary_mail(
            &fixture.active,
            &fixture.pending,
            session_id,
            &confirm_params(fixture, ids),
        )
        .await
        .map(|response| response.confirmed)
        .unwrap_or_default()
    }

    /// The provider accepted the hook context: Codex appends its developer
    /// `hooks.additional_context` item to the rollout.
    fn provider_accepts(fixture: &Fixture, mail: &BoundaryMail) {
        use std::io::Write as _;
        let block =
            rsi_common::boundary_mail_hook::render_mail_block(&mail.sender_role, &mail.text);
        let line = serde_json::json!({"type": "response_item", "payload": {
            "type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": block}],
            "internal_chat_message_metadata_passthrough":
                {"content_item_kinds": ["hooks.additional_context"]}}});
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.transcript)
            .expect("open transcript");
        writeln!(file, "{line}").expect("append");
    }

    /// Run the evidence watcher briefly.
    async fn watch(fixture: &Fixture, ids: &[Uuid]) {
        await_boundary_mail_evidence(
            &fixture.store,
            &fixture.pending,
            ids,
            tokio::time::Instant::now() + std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(10),
        )
        .await;
    }

    /// Claim, print, provider accepts: the full successful hand-off.
    async fn deliver(fixture: &Fixture, mail: &BoundaryMail) {
        assert_eq!(
            confirm(fixture, fixture.target, &[mail.message_id]).await,
            vec![mail.message_id]
        );
        provider_accepts(fixture, mail);
        watch(fixture, &[mail.message_id]).await;
    }

    /// Persist a provider assistant event on the target; `insert_event` runs
    /// the production acknowledgement pass (#46) over it.
    async fn persist_assistant_event(fixture: &Fixture, sequence: i32) {
        let guard = fixture.store.lock().await;
        guard
            .insert_event(&rsi_common::types::ConversationEvent {
                id: 0,
                session_id: fixture.target,
                sequence,
                event_type: rsi_common::types::EventType::Message,
                role: Some(rsi_common::types::Role::Assistant),
                content: "unrelated progress note".to_string(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .expect("insert event");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_pending_message_is_claimed_once_labelled_and_never_twice() {
        let fixture = fixture(true);
        let id = send(&fixture, "k1", "you are near your context wall").await;

        let first = claim(&fixture).await;
        assert_eq!(first.messages.len(), 1);
        let mail = &first.messages[0];
        assert_eq!(mail.message_id, id);
        assert_eq!(mail.sender_role, "manager");
        assert_eq!(mail.sender_session_id, Some(fixture.owner));
        assert!(mail.text.contains("you are near your context wall"));
        assert!(mail.text.contains(&fixture.owner.to_string()));
        // #1183: handed to the hook, printed, not yet delivered.
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");
        assert_eq!(confirm(&fixture, fixture.target, &[id]).await, vec![id]);
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");
        // Delivered once the provider transcript shows it accepted the context.
        provider_accepts(&fixture, mail);
        watch(&fixture, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "injected");

        // Delivered once: neither the next boundary nor the turn-end path can
        // claim it again.
        assert!(claim(&fixture).await.messages.is_empty());
        assert!(fixture.arbiter.roots_with_outstanding_grant().is_empty());
    }

    /// #1183 review: mail is `injected` only once the provider accepted the
    /// hook context, and only then can a later assistant event acknowledge it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn accepted_boundary_mail_is_acknowledged_by_the_next_assistant_event() {
        let fixture = fixture(true);
        let id = send(&fixture, "kc", "check the gate log").await;
        let claimed = claim(&fixture).await;
        deliver(&fixture, &claimed.messages[0]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "injected");
        persist_assistant_event(&fixture, 1).await;
        assert_eq!(
            message_state(&*fixture.store.lock().await, id),
            "acknowledged"
        );
        // A second confirmation is a no-op.
        assert!(confirm(&fixture, fixture.target, &[id]).await.is_empty());
    }

    /// #1183 review A: the hook printed and confirmed, but the provider timed
    /// the hook out (or rejected it) and discarded the output, so the
    /// transcript never shows it. The mail is never `injected` nor
    /// acknowledged by the next assistant event; the deadline settles it
    /// `uncertain`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn printed_mail_the_provider_discarded_settles_uncertain_never_acknowledged() {
        let fixture = fixture(true);
        let id = send(&fixture, "kd", "printed then discarded").await;
        let claimed = claim(&fixture).await;
        assert_eq!(confirm(&fixture, fixture.target, &[id]).await, vec![id]);
        // Context for a DIFFERENT message, and the model quoting this one, are
        // not evidence for this one.
        provider_accepts(
            &fixture,
            &BoundaryMail {
                text: "another message".into(),
                ..claimed.messages[0].clone()
            },
        );
        watch(&fixture, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");
        persist_assistant_event(&fixture, 1).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");

        settle_unconfirmed_boundary_mail(&fixture.store, &fixture.pending, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
        persist_assistant_event(&fixture, 2).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
    }

    /// #1183 review B: the provider accepted the context but the delivery
    /// record could not be persisted. The delivery stays pending (not lost
    /// from tracking), so the deadline still settles it `uncertain`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_delivery_that_cannot_be_persisted_stays_pending_and_settles_uncertain() {
        let fixture = fixture(true);
        let id = send(&fixture, "kp", "persist me").await;
        let claimed = claim(&fixture).await;
        fixture.pending.fail_next_record();
        deliver(&fixture, &claimed.messages[0]).await;
        // The failed record left it pending; the next poll in this watch
        // window recorded it. Prove the pending state survived the failure.
        assert_eq!(message_state(&*fixture.store.lock().await, id), "injected");

        let id = send(&fixture, "kp2", "persist me too").await;
        let claimed = claim(&fixture).await;
        assert_eq!(confirm(&fixture, fixture.target, &[id]).await, vec![id]);
        provider_accepts(&fixture, &claimed.messages[0]);
        // Every record in this window fails: the delivery is still tracked.
        fixture.pending.fail_next_record();
        await_boundary_mail_evidence(
            &fixture.store,
            &fixture.pending,
            &[id],
            tokio::time::Instant::now(),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");
        assert_eq!(fixture.pending.len(), 1);
        settle_unconfirmed_boundary_mail(&fixture.store, &fixture.pending, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
        assert_eq!(fixture.pending.len(), 0);
    }

    /// #1183 review B: a confirmation whose transcript locator is not this
    /// session's own provider conversation is refused with an error, and the
    /// delivery stays pending for the deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_unverifiable_confirmation_is_an_error_and_keeps_the_delivery_pending() {
        let fixture = fixture(true);
        let id = send(&fixture, "kv", "verify me").await;
        claim(&fixture).await;
        let other = fixture._dir.path().join("rollout-x-other-thread.jsonl");
        std::fs::write(&other, "").expect("other transcript");
        for (provider_session_id, path) in [
            (Some("other-thread"), Some(other.display().to_string())),
            (Some(THREAD), Some(other.display().to_string())),
            (Some(THREAD), Some(format!("rollout-x-{THREAD}.jsonl"))),
            (None, Some(fixture.transcript.display().to_string())),
            (Some(THREAD), None),
        ] {
            let params = rsi_common::boundary_mail_hook::ConfirmBoundaryMailParams {
                message_ids: vec![id],
                provider_session_id: provider_session_id.map(str::to_string),
                transcript_path: path.clone(),
            };
            let refused =
                confirm_boundary_mail(&fixture.active, &fixture.pending, fixture.target, &params)
                    .await;
            assert!(refused.is_err(), "{provider_session_id:?} {path:?}");
        }
        assert_eq!(fixture.pending.len(), 1);
        settle_unconfirmed_boundary_mail(&fixture.store, &fixture.pending, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
    }

    /// #1183 review: the hook claimed the mail and then died before printing.
    /// An unrelated assistant event must NOT acknowledge it; the missing
    /// confirmation settles it `uncertain` (visible, never re-sent), and a
    /// confirmation arriving after that changes nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_hook_crash_before_printing_is_settled_uncertain_never_acknowledged() {
        let fixture = fixture(true);
        let id = send(&fixture, "kx", "never printed").await;
        assert_eq!(claim(&fixture).await.messages[0].message_id, id);

        persist_assistant_event(&fixture, 1).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");

        // Deadline (or a reply that could not be written).
        settle_unconfirmed_boundary_mail(&fixture.store, &fixture.pending, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
        assert_eq!(fixture.pending.len(), 0);

        assert!(confirm(&fixture, fixture.target, &[id]).await.is_empty());
        persist_assistant_event(&fixture, 2).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
        assert!(claim(&fixture).await.messages.is_empty());
        // Settling again (the deadline after an early settle) is a no-op.
        settle_unconfirmed_boundary_mail(&fixture.store, &fixture.pending, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "uncertain");
    }

    /// A session can only confirm mail its own hook claimed, and a confirmed
    /// delivery is no longer settled by the deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn only_the_claiming_session_can_confirm_and_a_confirmed_delivery_survives_the_deadline()
    {
        let fixture = fixture(true);
        let id = send(&fixture, "ko", "mine").await;
        let claimed = claim(&fixture).await;
        let claim_mail_for = |_: &Fixture, _: Uuid| claimed.messages[0].clone();
        assert!(confirm(&fixture, fixture.owner, &[id]).await.is_empty());
        assert!(
            confirm(&fixture, fixture.target, &[Uuid::new_v4()])
                .await
                .is_empty()
        );
        assert_eq!(message_state(&*fixture.store.lock().await, id), "claimed");
        let mail = claim_mail_for(&fixture, id);
        deliver(&fixture, &mail).await;
        settle_unconfirmed_boundary_mail(&fixture.store, &fixture.pending, &[id]).await;
        assert_eq!(message_state(&*fixture.store.lock().await, id), "injected");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn agent_message_ids_reads_only_agent_deliveries() {
        let agent = Uuid::new_v4();
        let reply = serde_json::json!({"messages": [
            {"message_id": Uuid::new_v4(), "sender_role": "operator", "text": "a"},
            {"message_id": agent, "sender_role": "manager", "text": "b"},
        ]});
        assert_eq!(agent_message_ids(&reply), vec![agent]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn several_pending_messages_arrive_in_send_order_within_the_bound() {
        let fixture = fixture(true);
        let a = send(&fixture, "ka", "first").await;
        let b = send(&fixture, "kb", "second").await;
        let delivered = claim(&fixture).await;
        let ids: Vec<Uuid> = delivered.messages.iter().map(|m| m.message_id).collect();
        assert_eq!(ids, vec![a, b]);
        assert!(claim(&fixture).await.messages.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn without_a_running_turn_invocation_nothing_is_claimed_and_mail_stays_queued() {
        let fixture = fixture(false);
        let id = send(&fixture, "k2", "hold").await;
        assert!(claim(&fixture).await.messages.is_empty());
        assert_eq!(message_state(&*fixture.store.lock().await, id), "queued");
        assert!(fixture.arbiter.roots_with_outstanding_grant().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_session_that_is_not_running_claims_nothing() {
        let fixture = fixture(true);
        let id = send(&fixture, "k3", "hold").await;
        fixture.active.write().await.clear();
        assert!(claim(&fixture).await.messages.is_empty());
        assert_eq!(message_state(&*fixture.store.lock().await, id), "queued");
    }
    fn operator_state(store: &Store, id: Uuid) -> String {
        store
            .list_operator_messages(store_target(store, id))
            .expect("list")
            .into_iter()
            .find(|message| message.id == id)
            .expect("operator message")
            .state
    }

    fn store_target(store: &Store, id: Uuid) -> Uuid {
        let raw: String = store
            .conn
            .query_row(
                "SELECT session_id FROM operator_messages WHERE id=?1",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )
            .expect("session of operator message");
        Uuid::parse_str(&raw).expect("uuid")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_queued_operator_message_is_delivered_at_the_boundary_labelled_and_once() {
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "please also run the fmt check", "op-1")
            .expect("queue operator message");

        let first = claim(&fixture).await;
        assert_eq!(first.messages.len(), 1);
        assert_eq!(first.messages[0].message_id, operator.id);
        assert_eq!(first.messages[0].sender_role, "operator");
        assert_eq!(first.messages[0].sender_session_id, None);
        assert_eq!(first.messages[0].text, "please also run the fmt check");
        // Not `delivered` until the reply has been written (#1062): a death
        // now must surface as uncertain, never as delivered.
        let guard = fixture.store.lock().await;
        assert_eq!(operator_state(&guard, operator.id), "effect_possible");
        drop(guard);
        finalize(&fixture, &[operator.id]).await;
        let guard = fixture.store.lock().await;
        assert_eq!(operator_state(&guard, operator.id), "delivered");
        drop(guard);

        // At most once: neither the next boundary nor the turn-end dispatcher
        // (which claims through the same store call) can take it again.
        assert!(claim(&fixture).await.messages.is_empty());
        assert!(
            fixture
                .store
                .lock()
                .await
                .claim_operator_message(fixture.target)
                .expect("turn-end claim")
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn operator_messages_go_first_then_agent_mail_in_one_claim() {
        let fixture = fixture(true);
        let mail = send(&fixture, "km", "manager note").await;
        let first = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "operator one", "op-a")
            .expect("queue one");
        let second = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "operator two", "op-b")
            .expect("queue two");
        // One operator message per boundary (the open row blocks the next
        // claim until it settles), then agent mail.
        let claimed = claim(&fixture).await;
        let ids: Vec<Uuid> = claimed.messages.iter().map(|m| m.message_id).collect();
        assert_eq!(ids, vec![first.id, mail]);
        let roles: Vec<&str> = claimed
            .messages
            .iter()
            .map(|m| m.sender_role.as_str())
            .collect();
        assert_eq!(roles, vec!["operator", "manager"]);
        finalize(&fixture, &[first.id]).await;
        let next = claim(&fixture).await;
        let next_ids: Vec<Uuid> = next.messages.iter().map(|m| m.message_id).collect();
        assert_eq!(next_ids, vec![second.id]);
    }

    /// The RPC layer's step once the reply is written, plus the monitor's
    /// drain (the test has no monitor) with a counter starting at `sequence`.
    async fn finalize_with_sequence(fixture: &Fixture, ids: &[Uuid], sequence: &mut i32) {
        let bus = Arc::new(crate::bus::EventBus::new(10));
        finalize_operator_boundary_delivery(&fixture.store, &fixture.active, &bus, ids).await;
        let inbox = fixture
            .active
            .read()
            .await
            .get(&fixture.target)
            .map(|tracked| tracked.operator_inbox.clone());
        if let Some(inbox) = inbox {
            drain_operator_transcript_inbox(
                &inbox,
                &fixture.store,
                &fixture.active,
                &bus,
                fixture.target,
                sequence,
            )
            .await;
        }
    }

    async fn finalize(fixture: &Fixture, ids: &[Uuid]) {
        finalize_with_sequence(fixture, ids, &mut 0).await;
    }

    fn insert_plain_event(
        store: &Store,
        session_id: Uuid,
        sequence: i32,
        event_type: rsi_common::types::EventType,
        role: Option<rsi_common::types::Role>,
        content: &str,
    ) {
        store
            .insert_event(&rsi_common::types::ConversationEvent {
                id: 0,
                session_id,
                sequence,
                event_type,
                role,
                content: content.to_string(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .expect("insert event");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_delivered_operator_message_lands_between_the_tool_result_and_the_next_assistant_message()
     {
        use rsi_common::types::{EventType, Role};
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "stop and check the schema", "op-t")
            .expect("queue");
        {
            let guard = fixture.store.lock().await;
            insert_plain_event(
                &guard,
                fixture.target,
                1,
                EventType::Message,
                Some(Role::User),
                "task",
            );
            insert_plain_event(
                &guard,
                fixture.target,
                2,
                EventType::ToolUse,
                Some(Role::Assistant),
                "",
            );
            insert_plain_event(&guard, fixture.target, 3, EventType::ToolResult, None, "ok");
        }
        let claimed = claim(&fixture).await;
        assert_eq!(claimed.messages[0].message_id, operator.id);
        // The monitor's counter is at 3 (the tool result), so the operator
        // event is 4 and the next assistant message 5.
        let mut sequence = 3;
        finalize_with_sequence(&fixture, &[operator.id], &mut sequence).await;
        assert_eq!(sequence, 4);
        {
            let guard = fixture.store.lock().await;
            insert_plain_event(
                &guard,
                fixture.target,
                5,
                EventType::Message,
                Some(Role::Assistant),
                "on it",
            );
        }

        let guard = fixture.store.lock().await;
        let events = guard
            .load_events_since(fixture.target, None)
            .expect("events");
        let shape: Vec<(i32, EventType, Option<Role>)> = events
            .iter()
            .map(|event| (event.sequence, event.event_type, event.role))
            .collect();
        assert_eq!(
            shape,
            vec![
                (1, EventType::Message, Some(Role::User)),
                (2, EventType::ToolUse, Some(Role::Assistant)),
                (3, EventType::ToolResult, None),
                (4, EventType::Message, Some(Role::User)),
                (5, EventType::Message, Some(Role::Assistant)),
            ]
        );
        let delivered = &events[3];
        assert_eq!(delivered.content, "stop and check the schema");
        assert!(delivered.is_operator_message());
        assert!(!events[0].is_operator_message());
        assert_eq!(operator_state(&guard, operator.id), "delivered");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_delivery_finalized_twice_writes_exactly_one_transcript_event() {
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "once only", "op-x")
            .expect("queue");
        claim(&fixture).await;
        let mut sequence = 0;
        finalize_with_sequence(&fixture, &[operator.id], &mut sequence).await;
        finalize_with_sequence(&fixture, &[operator.id], &mut sequence).await;
        let guard = fixture.store.lock().await;
        let events = guard
            .load_events_since(fixture.target, None)
            .expect("events");
        assert_eq!(events.iter().filter(|e| e.is_operator_message()).count(), 1);
        assert_eq!(sequence, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_crash_between_the_claim_and_the_reply_write_is_uncertain_with_no_event() {
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "lost in the crash", "op-c")
            .expect("queue");
        // The claim ran, but the daemon died before the reply was written.
        assert_eq!(claim(&fixture).await.messages.len(), 1);
        let mut guard = fixture.store.lock().await;
        guard
            .reconcile_operator_messages_at_startup()
            .expect("startup reconcile");
        assert_eq!(operator_state(&guard, operator.id), "uncertain");
        assert!(
            guard
                .load_events_since(fixture.target, None)
                .expect("events")
                .iter()
                .all(|event| !event.is_operator_message())
        );
        // Uncertain is never re-sent nor settled `delivered` afterwards.
        assert!(
            guard
                .claim_operator_message(fixture.target)
                .expect("claim")
                .is_none()
        );
        assert!(
            guard
                .deliver_operator_message_with_transcript(operator.id, None)
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_reply_that_could_not_be_written_marks_the_message_uncertain() {
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "unsure", "op-w")
            .expect("queue");
        claim(&fixture).await;
        abandon_operator_boundary_delivery(&fixture.store, &[operator.id]).await;
        let guard = fixture.store.lock().await;
        assert_eq!(operator_state(&guard, operator.id), "uncertain");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn with_no_live_monitor_the_delivery_allocates_the_next_sequence_itself() {
        use rsi_common::types::{EventType, Role};
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "late", "op-n")
            .expect("queue");
        claim(&fixture).await;
        {
            let guard = fixture.store.lock().await;
            insert_plain_event(
                &guard,
                fixture.target,
                7,
                EventType::Message,
                Some(Role::Assistant),
                "x",
            );
        }
        fixture.active.write().await.clear();
        let bus = Arc::new(crate::bus::EventBus::new(10));
        finalize_operator_boundary_delivery(&fixture.store, &fixture.active, &bus, &[operator.id])
            .await;
        let guard = fixture.store.lock().await;
        let events = guard
            .load_events_since(fixture.target, None)
            .expect("events");
        assert_eq!(events.last().map(|event| event.sequence), Some(8));
        assert!(
            events
                .last()
                .is_some_and(|event| event.is_operator_message())
        );
        assert_eq!(operator_state(&guard, operator.id), "delivered");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn operator_message_ids_reads_only_operator_deliveries() {
        let operator = Uuid::new_v4();
        let reply = serde_json::to_value(ClaimBoundaryMailResponse {
            messages: vec![
                BoundaryMail {
                    message_id: operator,
                    sender_role: "operator".into(),
                    sender_session_id: None,
                    text: "a".into(),
                },
                BoundaryMail {
                    message_id: Uuid::new_v4(),
                    sender_role: "manager".into(),
                    sender_session_id: Some(Uuid::new_v4()),
                    text: "b".into(),
                },
            ],
        })
        .expect("json");
        assert_eq!(operator_message_ids(&reply), vec![operator]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_operator_message_for_a_session_that_is_not_running_stays_queued() {
        let fixture = fixture(true);
        let operator = fixture
            .store
            .lock()
            .await
            .queue_operator_message(fixture.target, "hold", "op-hold")
            .expect("queue");
        fixture.active.write().await.clear();
        assert!(claim(&fixture).await.messages.is_empty());
        let guard = fixture.store.lock().await;
        assert_eq!(operator_state(&guard, operator.id), "queued");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_operator_message_behind_an_uncertain_predecessor_is_not_claimed() {
        let fixture = fixture(true);
        let (uncertain, later) = {
            let mut guard = fixture.store.lock().await;
            let first = guard
                .queue_operator_message(fixture.target, "first", "op-1")
                .expect("first");
            let later = guard
                .queue_operator_message(fixture.target, "second", "op-2")
                .expect("second");
            guard.claim_operator_message(fixture.target).expect("claim");
            guard
                .mark_operator_message_effect_possible(first.id)
                .expect("effect");
            guard
                .settle_operator_message(first.id, false)
                .expect("uncertain");
            (first, later)
        };
        assert!(claim(&fixture).await.messages.is_empty());
        let guard = fixture.store.lock().await;
        assert_eq!(operator_state(&guard, uncertain.id), "uncertain");
        assert_eq!(operator_state(&guard, later.id), "queued");
    }
}
