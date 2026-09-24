//! P2-05b — native agent-message dispatch at the monitor's idle boundary.
//!
//! This module is the production consumer of everything P2-01 through P2-04
//! built but left unwired: it is the first code in the tree that actually
//! delivers an agent message to a provider.
//!
//! # Why this lives beside `monitor.rs` rather than inside it
//!
//! `monitor.rs` runs for EVERY session of EVERY provider. The delivery
//! sequence is long, and inlining it would put a hundred lines of
//! message-specific control flow into the daemon's hottest shared path, where
//! every future reader of the monitor loop has to page it in. Keeping it here
//! holds `monitor.rs`'s diff to the arbiter block it already owns, and gives
//! these transactions a testable seam that does not require a live monitor.
//!
//! # The ordering, and the one rule it exists to honour
//!
//! > **No external effect may occur inside a SQLite transaction.**
//!
//! SQLite owns durable intent, CAS, and evidence; provider dispatch is an
//! external effect. So the sequence is three *separate* transactions with the
//! effect strictly between the second and the third, and no store guard is ever
//! held across an `.await` that can reach a provider:
//!
//! | # | Step | Transaction | External effect |
//! |---|---|---|---|
//! | 1 | `admit_invocation` | **TX-A**, opened and committed internally | no |
//! | 2 | `claim_agent_message_exact` | **TX-B**, one `BEGIN IMMEDIATE` | no |
//! | 3 | `start_admitted_message_turn` | **none — guard already dropped** | **YES** |
//! | 4 | `record_agent_message_admission` | **TX-C**, one `BEGIN IMMEDIATE` | no |
//!
//! TX-A must precede TX-B because TX-B *refuses* to claim unless the
//! invocation row already exists — it returns
//! `CasLost(ModelInvocationMissing)` rather than trusting the caller. And
//! `claim_agent_message_exact` mandates the rest itself: *"Dispatch is
//! deliberately NOT part of this function ... The caller dispatches only after
//! this commits."*
//!
//! # Settlement ownership — the failure mode this module is shaped around
//!
//! Once TX-A commits, three parties could settle the same invocation: an
//! explicit `fail`, `ModelCallSettlement::drop`, and the store-backed call
//! control's own `Drop`. Because `complete`/`fail` only disarm on a SUCCESSFUL
//! store write, letting a settlement fall out of scope silently reclassifies a
//! provably clean pre-effect abort as `model_call_task_exited` — a wrong error
//! class on a path that had no effect at all.
//!
//! **The rule, applied at every exit below: on every non-dispatch path, MOVE
//! the settlement into an explicit failure with a pre-effect error class.
//! `Drop` is the safety net, never the plan.** [`settle_pre_effect`] is the one
//! helper that does it, so the rule is enforced in one place rather than
//! restated at each `return`.
//!
//! ## Every non-dispatch exit, and how each one settles
//!
//! Enumerated exhaustively because "some exit forgets to settle" is the single
//! most likely way to get this module wrong, and a table is auditable where a
//! reading of the control flow is not.
//!
//! | # | Exit | Settles how |
//! |---|---|---|
//! | 1 | session no longer in `active` | **nothing to settle** — no permit minted yet |
//! | 2 | attempt-number overflow | **nothing to settle** — no permit minted yet |
//! | 3 | `admit_invocation` → `Err` (budget denial, kill switch) | **nothing to settle** — no permit was minted; message stays `queued` |
//! | 4 | `admit_invocation` → `Duplicate` | **nothing to settle** — no permit is handed back |
//! | 5 | `AdmittedModelCall::real` → `Err` | **`Drop` net, forced.** The API moves the permit in and does not return it on failure, so there is nothing to settle explicitly. Identical to the ordinary `start_turn`. |
//! | 6 | `claim_agent_message_exact` → `CasLost` | **explicit** — `agent_message_claim_cas_lost` |
//! | 7 | `claim_agent_message_exact` → `Err` | **explicit** — `agent_message_claim_store_error` |
//! | 8 | fence/invocation pre-check mismatch | **explicit** — `agent_message_native_turn_invocation_fence_mismatch`, plus a recorded `rejected_before_effect` |
//! | 9 | `NativeMessageTurnRequest::new` → `Err` | **`Drop` net, forced** (`new` consumes the settlement). Made unreachable by exit 8's pre-check, and handled rather than `expect`ed because a panic here would strand a committed claim. |
//! | 10 | provider → `RejectedBeforeEffect` | **explicit** — the provider's own `&'static str` class, plus a recorded `rejected_before_effect` |
//! | 11 | provider → `Unsupported` | **explicit** — `agent_message_provider_unsupported`, plus a recorded `unsupported` |
//! | 12 | provider → `Err` (unknown effect) | **`Drop` net, deliberately.** The settlement is inside the provider and the effect is unknown, so the conservative `model_call_task_exited` is the honest class; the attempt is recorded `admitted_effect_possible` and the session goes terminal. |
//!
//! Exits 5 and 9 are `Drop`-net because the existing API shape forces it, not
//! because it was chosen; exit 12 is `Drop`-net *by design*. Every other
//! non-dispatch exit settles explicitly.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use rsi_common::agent_coordination::{
    BoundaryAdmissionV1, BoundaryClassificationV1, MessageAttemptFenceV1,
};
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelUsageConfidence};

use crate::bus::EventBus;
use crate::model_control::call_control::{AdmittedModelCall, ModelCallSettlementHandle};
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{
    AdmissionDecision, InvocationCompletion, ModelAdmissionRequest, admit_invocation,
    explicit_expected_usage, hash_request_fingerprint,
};
use crate::provider::{
    AdmittedMessageTurnOutcome, NativeMessageTurnRequest, ProviderSession, TurnConfig,
};
use crate::store::Store;
use crate::store::agent_coordination::{
    ClaimAgentMessageOutcome, ClaimAgentMessageRequest, NoEffectDisposition, RecordAdmissionOutcome,
};

use super::agent_message_arbiter::ArbitrationGrant;
use super::types::TrackedSession;

/// How long one delivery claim's lease runs.
///
/// This is a RECOVERY SCAN WINDOW, not a kill timer, and the distinction is the
/// whole reason the value is not tuned tightly. Nothing cancels a turn when the
/// lease lapses: the plan is explicit that *"lease expiry alone never
/// authorizes a second external call"*, and P2-06 may not requeue a claimed
/// attempt on expiry alone — it needs positive proof of no effect. So the lease
/// only bounds how long recovery waits before *examining* an attempt.
///
/// Thirty minutes is chosen as a generous upper bound on one provider turn. Too
/// short would make reconciliation examine healthy in-flight turns constantly;
/// too long would delay noticing genuinely stuck ones. Neither error can cause
/// a double send, which is why a coarse value is acceptable here.
const DELIVERY_CLAIM_LEASE_SECS: i64 = 30 * 60;

/// The `trigger` recorded on every delivery invocation.
///
/// This is half of how a delivery turn stays distinguishable from an ordinary
/// synthetic continuation in the ledger without minting a new
/// `ModelInvocationPurpose` — the other half is the attempt row's
/// `delivery_model_invocation_id` join. See [`build_admission_request`].
const DELIVERY_TRIGGER: &str = "agent_message_delivery";

/// What the monitor should do after a delivery attempt at one idle boundary.
///
/// Total by construction: every path through [`deliver_at_idle_boundary`] ends
/// in exactly one of these, so the monitor's own `match` cannot silently omit a
/// case.
#[derive(Debug)]
pub(crate) enum IdleBoundaryDelivery {
    /// A message turn reached the provider and IS this session's next turn. The
    /// caller must NOT also start a synthetic continuation, and must keep the
    /// grant outstanding for the duration of that turn.
    Dispatched,
    /// Nothing was dispatched and nothing could have been: no capability was
    /// consumed and no byte was queued. The caller runs the ordinary synthetic
    /// continuation exactly as it did before this slice existed.
    FellBackToSyntheticContinuation { reason: &'static str },
    /// A dispatch may have reached the provider but did not complete cleanly.
    ///
    /// The session must go terminal. The plan forbids the alternative in as
    /// many words: an effect-possible attempt *"cannot restore, retry, or start
    /// the synthetic continuation."* Starting one here would run a second turn
    /// on a provider that may already be acting on the first.
    EffectPossibleTerminal { error_class: &'static str },
}

/// Provider facts the admission request needs, snapshotted from the live
/// session under a read guard that is released before any transaction.
struct DeliveryFacts {
    project_id: Option<Uuid>,
    model: Option<String>,
    effort: Option<String>,
    working_dir: std::path::PathBuf,
}

/// Settle an admitted-but-never-dispatched invocation EXPLICITLY.
///
/// Every non-dispatch exit funnels through here. The error class is always a
/// `&'static str` from a closed vocabulary, so no provider payload and no
/// formatted sender text can reach the durable row.
///
/// A failure to settle is logged rather than propagated: by the time this runs
/// the caller has already decided the delivery outcome, and the `Drop` net
/// still fires because `settle_result` only disarms the permit on a successful
/// store write. Turning a settlement hiccup into a monitor error would convert
/// a harmless bookkeeping retry into a terminal session.
async fn settle_pre_effect(
    settlement: crate::model_control::call_control::ModelCallSettlement,
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    error_class: &'static str,
) {
    let completion = InvocationCompletion {
        error_class: Some(error_class.to_string()),
        confidence: Some(ModelUsageConfidence::Unavailable),
        ..InvocationCompletion::default()
    };
    if let Err(error) = settlement
        .settle_result::<()>(
            store,
            event_bus,
            completion,
            Ok(()),
            "agent message delivery pre-effect settlement",
        )
        .await
    {
        tracing::warn!(
            target: "agent_coordination",
            error_class,
            error = %error,
            "failed to settle an undispatched agent-message delivery invocation; \
             the drop-based net remains armed because the permit was not disarmed"
        );
    }
}

/// Build the admission request for one delivery turn.
///
/// # Why this reuses `SessionCodexAppServerTurn` instead of minting a purpose
///
/// A delivery turn is policy-equivalent to an ordinary turn on every axis the
/// registry row actually encodes: the same auth/billing path, `PaidCapable`,
/// `Foreground`, per-turn budget enforcement, and the same kill-switch
/// coverage. It is a `turn/start` on the same transport. Minting a second
/// purpose would add compile-forced churn across every exhaustive match in the
/// frozen registry to buy `purpose`-column filtering that is already
/// recoverable two other ways: the attempt row's
/// `delivery_model_invocation_id` join, and this request's [`DELIVERY_TRIGGER`]
/// and `dedup_key`.
///
/// **A delivery turn is a real paid model call and does not escape
/// accounting.** Flowing through `admit_invocation` gives it a durable
/// `model_invocations` row, tier classification, store-side budget enforcement
/// that can deny it outright, budget alerts, and measured settlement — exactly
/// what an ordinary turn gets.
fn build_admission_request(
    session_id: Uuid,
    facts: &DeliveryFacts,
    message_id: Uuid,
    next_attempt_number: u32,
) -> ModelAdmissionRequest {
    let purpose = ModelInvocationPurpose::SessionCodexAppServerTurn;
    // Unique per ATTEMPT, not per message: a later attempt at the same message
    // is a genuinely new paid call and must admit its own row rather than
    // colliding with the previous attempt's dedup key and being refused.
    let dedup_key = format!("{DELIVERY_TRIGGER}:{message_id}:{next_attempt_number}");
    let fingerprint = hash_request_fingerprint(&[
        DELIVERY_TRIGGER,
        &session_id.to_string(),
        &message_id.to_string(),
        &next_attempt_number.to_string(),
    ]);
    ModelAdmissionRequest {
        purpose,
        // Mirrors the labels `launch.rs` uses when it builds this session's own
        // `StoreBackedModelCallControl`, so a delivery turn and an ordinary
        // turn classify into the same model tier and bill the same way.
        provider: Some("CodexAppServer".to_string()),
        model: facts.model.clone(),
        backend: Some("CodexAppServer".to_string()),
        effort: facts.effort.clone(),
        trigger: DELIVERY_TRIGGER.to_string(),
        owner: InvocationOwner {
            session_id: Some(session_id),
            project_id: facts.project_id,
            ..Default::default()
        },
        dedup_key: Some(dedup_key),
        request_fingerprint: Some(fingerprint),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(explicit_expected_usage(
            purpose,
            Some("CodexAppServer"),
            Some("CodexAppServer"),
            facts.model.as_deref(),
        )),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    }
}

/// Assemble the exact claim fence from the grant plus this attempt's admitted
/// invocation.
///
/// Every `expected_*` value comes from the grant rather than from a fresh read,
/// which is the point: they are assertions the claim re-checks under
/// `BEGIN IMMEDIATE`, so re-reading them here would launder a stale snapshot
/// into a fence that always agrees with itself.
fn build_claim_request(
    grant: &ArbitrationGrant,
    delivery_model_invocation_id: Uuid,
    delivery_boot_id: Uuid,
) -> ClaimAgentMessageRequest {
    let request = grant.request();
    ClaimAgentMessageRequest {
        message_id: request.message_id,
        owner_session_id: request.owner_session_id,
        logical_root_session_id: request.logical_root_session_id,
        expected_state_version: request.expected_state_version,
        expected_current_attempt_number: request.expected_current_attempt_number,
        delivery_session_id: request.delivery_session_id,
        expected_session_generation: request.expected_generation,
        expected_prior_model_invocation_id: request.expected_prior_model_invocation_id,
        delivery_model_invocation_id,
        delivery_boot_id,
        claim_token: Uuid::new_v4(),
        provider_kind: request.provider_kind,
        claim_expires_at: Utc::now() + chrono::Duration::seconds(DELIVERY_CLAIM_LEASE_SECS),
        // `authority_kind` is fixed to `'dispatcher'` by the claim statement;
        // the monitor that owns this idle boundary IS that dispatcher, so its
        // delivery Session id is the honest capability identity. It also stays
        // within the 36-byte capability-id bound by construction.
        authority_id: request.delivery_session_id,
    }
}

/// Persist the boundary admission (TX-C) and report whether it stuck.
///
/// Failing to record is deliberately NOT escalated into a monitor error. After
/// a successful dispatch the effect has already happened; the attempt simply
/// stays `claimed` and P2-06's reconciliation must resolve it — and crucially
/// it will resolve it as `uncertain` rather than requeueing, because the boot
/// id on the row MATCHES this live incarnation. That is the correct
/// conservative answer, so a loud log is the right response rather than tearing
/// down a session whose turn is genuinely running.
async fn record_admission(
    store: &Arc<Mutex<Store>>,
    fence: &MessageAttemptFenceV1,
    admission: &BoundaryAdmissionV1,
    disposition: NoEffectDisposition,
    authority_id: Uuid,
) {
    let outcome = {
        let guard = store.lock().await;
        guard.record_agent_message_admission(fence, admission, disposition, authority_id)
    };
    match outcome {
        Ok(RecordAdmissionOutcome::Recorded { state, .. }) => {
            tracing::debug!(
                target: "agent_coordination",
                message_id = %fence.message_id,
                attempt_number = fence.attempt_number,
                classification = admission.classification.as_str(),
                state = state.as_str(),
                "recorded agent-message boundary admission"
            );
        }
        Ok(RecordAdmissionOutcome::FenceLost) => {
            tracing::warn!(
                target: "agent_coordination",
                message_id = %fence.message_id,
                attempt_number = fence.attempt_number,
                "agent-message admission fence was lost before it could be recorded; \
                 something else already settled this attempt"
            );
        }
        Err(error) => {
            tracing::error!(
                target: "agent_coordination",
                message_id = %fence.message_id,
                attempt_number = fence.attempt_number,
                classification = admission.classification.as_str(),
                error = %error,
                "failed to record an agent-message boundary admission; the attempt stays \
                 claimed under THIS incarnation's boot id, so recovery must classify it \
                 uncertain rather than requeue it"
            );
        }
    }
}

/// Deliver at most one agent message at one idle result boundary.
///
/// The caller owns the grant. On [`IdleBoundaryDelivery::Dispatched`] it must
/// KEEP it outstanding for the duration of the delivered turn — that is what
/// makes "one outstanding grant blocks every other `start_turn`" true of the
/// running system rather than only of the registry. On every other outcome the
/// caller releases it.
pub(crate) async fn deliver_at_idle_boundary(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    settlements: &ModelCallSettlementHandle,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    session_id: Uuid,
    grant: &ArbitrationGrant,
    provider_session: &mut dyn ProviderSession,
) -> IdleBoundaryDelivery {
    let message_id = grant.message_id();

    // ---- 0. snapshot session facts; guard released immediately -----------
    let Some(facts) = ({
        let guard = active.read().await;
        guard.get(&session_id).map(|tracked| DeliveryFacts {
            project_id: tracked.session.project_id,
            model: tracked.session.model.clone(),
            effort: tracked.session.effort.clone(),
            working_dir: tracked.session.working_dir.clone(),
        })
    }) else {
        return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
            reason: "delivery session is no longer active",
        };
    };

    let Some(next_attempt_number) = grant
        .request()
        .expected_current_attempt_number
        .unwrap_or(0)
        .checked_add(1)
    else {
        return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
            reason: "agent message attempt counter would overflow",
        };
    };

    // ---- 1. TX-A: admit the durable invocation ---------------------------
    //
    // This MUST precede the claim: the claim refuses with a typed
    // `ModelInvocationMissing` CAS loss unless this row is already committed.
    let request = build_admission_request(session_id, &facts, message_id, next_attempt_number);
    let permit = match admit_invocation(store, request, event_bus).await {
        Ok(AdmissionDecision::Admitted(permit)) => permit,
        // A budget denial or kill switch. No permit was minted, so there is
        // nothing to settle, and no claim happened, so the message is still
        // `queued` and will be retried at a later boundary.
        Err(error) => {
            tracing::warn!(
                target: "agent_coordination",
                session_id = %session_id,
                message_id = %message_id,
                error = %error,
                "agent-message delivery was denied admission; the message stays queued"
            );
            return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "delivery invocation was denied admission",
            };
        }
        Ok(AdmissionDecision::Duplicate { invocation_id }) => {
            tracing::warn!(
                target: "agent_coordination",
                session_id = %session_id,
                message_id = %message_id,
                %invocation_id,
                "agent-message delivery admission collided with an existing dedup key"
            );
            return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "delivery invocation dedup key already existed",
            };
        }
    };

    // ---- 2. claim the one-use execution capability -----------------------
    //
    // The route is not a free choice. `purpose_allows_runtime_route` resolves
    // `SessionCodexAppServerTurn` to the `appserver_model_request` boundary,
    // and that boundary's row carries exactly this runtime route, so any other
    // value here is a policy denial rather than a different transport.
    //
    // NOTE on settlement: `AdmittedModelCall::real` moves the permit into a
    // settlement BEFORE claiming, and does not hand it back on failure. This
    // is the one place the explicit-settlement rule cannot be applied, because
    // the API gives the caller nothing to settle; the `ModelCallSettlement`
    // drop net covers it, exactly as it does for the ordinary `start_turn`.
    let (settlement, execution) =
        match AdmittedModelCall::real(permit, RuntimeExecutionRoute::CodexAppServer, {
            settlements.clone()
        }) {
            Ok(call) => call.into_parts(),
            Err(error) => {
                tracing::warn!(
                    target: "agent_coordination",
                    session_id = %session_id,
                    message_id = %message_id,
                    error = %error,
                    "agent-message delivery could not claim its execution capability"
                );
                return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                    reason: "delivery execution capability was refused",
                };
            }
        };

    // ---- 3. TX-B: the ONE claim transaction ------------------------------
    //
    // The guard is scoped to this block and dropped before the dispatch below.
    // That scoping is the whole invariant-1 discipline on this path: unlike the
    // arbiter call above — which is a plain `fn` whose result borrows nothing —
    // nothing structurally prevents holding this guard across the provider
    // await, so it is prevented explicitly.
    let delivery_boot_id = {
        let guard = store.lock().await;
        guard.delivery_boot_id()
    };
    let claim = build_claim_request(
        grant,
        settlement
            .invocation_id()
            .expect("a real delivery settlement retains its permit"),
        delivery_boot_id,
    );
    let authority_id = claim.authority_id;
    let claim_outcome = {
        let guard = store.lock().await;
        guard.claim_agent_message_exact(&claim)
    };

    let fence = match claim_outcome {
        Ok(ClaimAgentMessageOutcome::Claimed(fence)) => fence,
        // A CAS loss is ordinary control flow, not a fault: nothing was
        // written, so there is zero attempt, zero transition, zero Session
        // binding, zero watch and zero external effect. The obligation is to
        // settle the now-unused invocation with a pre-effect class.
        Ok(ClaimAgentMessageOutcome::CasLost(loss)) => {
            tracing::info!(
                target: "agent_coordination",
                session_id = %session_id,
                message_id = %message_id,
                cas_loss_class = loss.as_str(),
                "agent-message delivery lost its claim CAS; settling the unused invocation"
            );
            settle_pre_effect(settlement, store, event_bus, "agent_message_claim_cas_lost").await;
            return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "delivery claim lost its CAS",
            };
        }
        Err(error) => {
            tracing::warn!(
                target: "agent_coordination",
                session_id = %session_id,
                message_id = %message_id,
                error = %error,
                "agent-message delivery claim failed; settling the unused invocation"
            );
            settle_pre_effect(
                settlement,
                store,
                event_bus,
                "agent_message_claim_store_error",
            )
            .await;
            return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "delivery claim transaction failed",
            };
        }
    };

    // ══════════════════════════════════════════════════════════════════════
    // THE CRASH-BETWEEN-COMMIT-AND-DISPATCH WINDOW
    //
    // TX-B has committed. The store guard is dropped. The dispatch below has
    // NOT happened. If the daemon dies right here, the attempt row survives as
    // `claimed` and NOTHING durable records that the dispatch never occurred —
    // the process died before it could write anything.
    //
    // That would strand the message forever without a rule, because P2-06 may
    // move `claimed → queued` only with recorded `rejected_before_effect` plus
    // `proved_no_effect`, states "lease expiry alone is never this proof", and
    // does not admit `uncertain → queued` at all.
    //
    // THE RULE THIS SLICE ESTABLISHES:
    //
    //   An attempt that is `claimed`, whose `delivery_boot_id` differs from the
    //   live daemon's `Store::delivery_boot_id()`, and which carries no
    //   recorded admission, is `uncertain`. It is NOT proof of no effect and
    //   MUST NOT be requeued.
    //
    // ─── CORRECTED BY REVIEW R5 (H21-P2-R5-001, fix option (ii)) ───────────
    //
    // This comment previously stated the OPPOSITE conclusion — that the same
    // three conjuncts PROVED no external effect and licensed a requeue. That
    // was UNSOUND, and the original reasoning is preserved below so the defect
    // stays legible rather than being quietly overwritten:
    //
    //   > It is sound because of the ordering above, not by assertion:
    //   >   1. the send is strictly AFTER the claim commit, and
    //   >   2. a boot-id mismatch means the incarnation that wrote the row is
    //   >      dead, and a dead incarnation cannot have an in-flight send.
    //
    // BOTH HALVES ARE INDIVIDUALLY TRUE. They do not compose into the stated
    // conclusion. Half 1 rules out an effect BEFORE the claim commit. Half 2
    // rules out an effect IN FLIGHT at the moment of recovery. Neither rules
    // out the case that actually matters: **an effect that COMPLETED and was
    // then never durably recorded.**
    //
    // The durable record of the effect is written AFTER the effect. The send is
    // at step 5 below; its first durable trace is the TX-C `record_admission`
    // in step 6. `start_admitted_message_turn` returns `Dispatched` on
    // `AppServerDispatchOutcome::EnqueuedWithoutReceipt`, whose own contract is
    // that enqueue is NOT completion — the writer task may already have written
    // those bytes. So from the send onward an effect is possible, and it stays
    // durably INVISIBLE until TX-C commits.
    //
    // Therefore the interval (send, TX-C commit) leaves behind EXACTLY the same
    // durable triple as a genuine pre-dispatch crash: attempt `claimed`, boot id
    // foreign after any restart, no recorded admission. **The two windows are
    // durably indistinguishable**, so this rule cannot discriminate "never sent"
    // from "sent but never recorded", and must therefore answer `uncertain`.
    //
    // ─── STRUCTURAL FIX LANDED (H21-P2-R5-001 option (i)) ─────────────────
    //
    // Everything above describes the defect as it stood BEFORE step 4b below.
    // The discriminator that was missing now exists: `AttemptStateV1` carries
    // `Dispatching`, and step 4b commits it in its own short transaction that
    // closes strictly before the send. The two windows are therefore no longer
    // durably identical:
    //
    //   `claimed`     + foreign boot + no admission ⇒ PROVED NO EFFECT — the
    //                   dead incarnation never reached the marker, so it never
    //                   reached the send.
    //   `dispatching` + foreign boot + no admission ⇒ UNCERTAIN — the send may
    //                   have completed and never been recorded. NEVER requeue.
    //
    // The proof rests on an ORDERING that step 4b maintains in both directions:
    // the marker commits before the send, AND the send is refused outright if
    // the marker cannot be made durable. Moving the marker after the send, or
    // making its failure non-fatal, silently restores the original defect.
    //
    // NOTE — this slice builds the EVIDENCE ONLY. The recovery that reads it is
    // P2-06's and is deliberately not implemented here.
    //
    // What was true before the fix, retained because it still constrains any
    // ALTERNATIVE discriminator someone might reach for:
    //   - Between the TX-B commit above and the send below, the only other
    //     store touch is the `delivery_boot_id` READ, which happens before the
    //     claim and so cannot discriminate.
    //   - The `model_invocations` row does not discriminate: on the dispatched
    //     path the settlement moves into the provider's `current_turn_call`
    //     slot, leaving the invocation admitted-and-unsettled — the identical
    //     state it holds between the claim commit and the send.
    //
    // THREE WAYS INTO THAT WINDOW, and one is not even a crash:
    //   (a) Hard crash / SIGKILL between the send and TX-C. `record_admission`
    //       takes `store.lock().await` and writes — a real await plus disk I/O.
    //   (b) A `record_admission` store error AFTER a successful dispatch. Its
    //       `Err` arm deliberately swallows the error, reasoning that the row
    //       stays `claimed` with a MATCHING boot id. That is true only for the
    //       remaining lifetime of THIS incarnation; after any subsequent daemon
    //       restart — including a clean, planned one — the boot id is foreign
    //       and the row satisfies all three conjuncts. A tolerated, logged,
    //       "conservative" error path therefore MATURES into a false no-effect
    //       proof at the next restart.
    //   (c) The `EffectPossibleTerminal` arm, which explicitly acknowledges the
    //       effect may have occurred, has the same shape: if its
    //       `record_admission` fails, the row is left `claimed` and unrecorded.
    //
    // CONSEQUENCE FOR P2-06 — THE STRUCTURAL BLOCKER IS CLEARED, THE RULE IS
    // NARROWED:
    //
    //   P2-06 STILL MUST NOT implement the withdrawn rule. `claimed` alone was
    //   never the discriminator; conditioning a requeue on it would redeliver a
    //   message the provider has already received — a double-delivered paid
    //   model turn, the exact outcome this phase's invariant structure exists
    //   to prevent.
    //
    //   What P2-06 MAY now rely on is the NARROWER pair above, and only because
    //   step 4b makes `dispatching` durable before the send. The requeue is
    //   licensed for `claimed` + foreign boot + no admission; it remains
    //   FORBIDDEN for `dispatching` + foreign boot, which stays `uncertain`.
    //
    //   `dispatching` crash-during-delivery messages therefore still STRAND as
    //   `uncertain`. That is the correct conservative answer and is exactly what
    //   the plan means by "Generic `Err`, lease expiry, missing output, process
    //   death, or timeout is never proof of no effect." **Stranding is
    //   operator-recoverable; a double-delivered paid model turn is not.**
    //
    // The design stated the STRONGER rule that was silently weakened: it
    // required "zero provider-request rows" — a durable artifact of the SEND —
    // as the third conjunct. This slice substituted "no recorded admission",
    // which is written AFTER the send and therefore cannot discriminate.
    //
    // WHAT REMAINS PINNED, stated precisely rather than overclaimed: the
    // ORDERING (the send is strictly after the claim commit) is genuinely
    // pinned by
    // `session::agent_message_delivery::tests::a_claim_commits_before_any_provider_dispatch`,
    // and it stays load-bearing — if a future change moves a provider send
    // above this line, even the `uncertain` classification degrades.
    // `session::agent_message_delivery::tests::a_delivery_attempt_is_stamped_with_the_live_daemon_boot_id`
    // asserts the rule's PRECONDITION (the stamped id is the live, non-nil
    // daemon identity). It was previously named
    // `a_foreign_boot_id_on_an_unrecorded_claimed_attempt_proves_no_effect`,
    // which named the very conclusion R5 withdrew; the name now states only
    // what the body actually pins.
    // ══════════════════════════════════════════════════════════════════════

    // ---- 4. bind the fence to the capability -----------------------------
    //
    // Pre-checked rather than relying on `new`'s error, because `new` consumes
    // the settlement and cannot hand it back — and a post-claim exit that
    // settles by `Drop` would both mislabel the error class and strand a
    // `claimed` attempt.
    if settlement.invocation_id() != Some(fence.delivery_model_invocation_id) {
        settle_pre_effect(
            settlement,
            store,
            event_bus,
            "agent_message_native_turn_invocation_fence_mismatch",
        )
        .await;
        record_admission(
            store,
            &fence,
            &rejected_admission(
                grant,
                &fence,
                "agent_message_native_turn_invocation_fence_mismatch",
            ),
            NoEffectDisposition::Requeue,
            authority_id,
        )
        .await;
        return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
            reason: "delivery fence did not match its invocation",
        };
    }
    // ---- 4b. TX-B2: the durable PRE-DISPATCH marker ----------------------
    //
    // This closes the crash window documented above (H21-P2-R5-001 option (i)).
    // It is its own SHORT transaction, taken and released here, and it commits
    // strictly BEFORE the send in step 5 — which is the entire point. A marker
    // that landed after the send, or that was skipped when it failed, would
    // prove nothing and leave the two windows durably identical again.
    //
    // Placed before the turn bind rather than immediately above the send for a
    // concrete reason: `NativeMessageTurnRequest::new` CONSUMES `settlement`,
    // so a failure after the bind could not settle with a pre-effect class
    // without unwinding the turn. The bind is pure in-memory construction and
    // cannot produce an external effect, so committing the marker just above it
    // still satisfies "durable strictly before any possible effect", which is
    // the property the recovery rule actually rests on.
    //
    // ON FAILURE WE DO NOT SEND. Dispatching without a durable marker would
    // manufacture precisely the ambiguity this slice exists to remove: the row
    // would stay `claimed` while a paid model turn went out, and a later
    // recovery reading `claimed` as proof-of-no-effect would redeliver it. A
    // message that is not sent at all is recoverable; one that is sent
    // invisibly is not. So this settles pre-effect and requeues — a REAL
    // proved-no-effect, because the send provably never happened.
    let marked = {
        let guard = store.lock().await;
        guard.mark_agent_message_attempt_dispatching(&fence)
    };
    if let Err(error) = marked {
        tracing::warn!(
            target: "agent_coordination",
            session_id = %session_id,
            message_id = %message_id,
            error = %error,
            "agent-message delivery could not durably mark dispatch; refusing to send"
        );
        settle_pre_effect(
            settlement,
            store,
            event_bus,
            "agent_message_dispatch_marker_failed",
        )
        .await;
        record_admission(
            store,
            &fence,
            &rejected_admission(grant, &fence, "agent_message_dispatch_marker_failed"),
            NoEffectDisposition::Requeue,
            authority_id,
        )
        .await;
        return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
            reason: "delivery could not durably mark its dispatch",
        };
    }

    let turn = match NativeMessageTurnRequest::new(fence.clone(), settlement, execution) {
        Ok(turn) => turn,
        Err(class) => {
            // Unreachable given the pre-check above, and handled anyway rather
            // than `expect`ed: this sits after a committed claim, where a panic
            // would leave the attempt permanently claimed.
            tracing::error!(
                target: "agent_coordination",
                message_id = %message_id,
                class,
                "agent-message turn refused its own fence after a successful pre-check"
            );
            record_admission(
                store,
                &fence,
                &rejected_admission(grant, &fence, "agent_message_native_turn_bind_failed"),
                NoEffectDisposition::Requeue,
                authority_id,
            )
            .await;
            return IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "delivery turn refused its fence",
            };
        }
    };

    // ---- 5. THE EXTERNAL EFFECT — no store guard is held here -------------
    //
    // `render_payload_for_delivery` is the ONLY rendering on this path. It
    // routes through `wrap_agent_message`, whose envelope neutralization makes
    // forging a `rsid-daemon-message` boundary structurally unreachable. There
    // is deliberately no accessor that returns the sender's raw bytes, and this
    // slice — the first to actually deliver a payload — must not add one.
    let config = TurnConfig {
        input: grant.render_payload_for_delivery(),
        working_dir: Some(facts.working_dir),
    };
    let outcome = provider_session
        .start_admitted_message_turn(turn, &config)
        .await;

    // ---- 6. TX-C: record what the boundary actually did -------------------
    match outcome {
        Ok(AdmittedMessageTurnOutcome::Dispatched { native_turn_id }) => {
            let admission = BoundaryAdmissionV1 {
                provider_kind: grant.request().provider_kind,
                capability_kind: grant.request().provider_kind.capability_kind(),
                delivery_session_id: fence.delivery_session_id,
                session_generation: fence.delivery_session_generation,
                model_invocation_id: fence.delivery_model_invocation_id,
                native_turn_id,
                classification: BoundaryClassificationV1::AdmittedEffectPossible,
                provider_error_class: None,
            };
            // `NoEffectDisposition` is INERT here, and that is checked rather
            // than assumed: `record_agent_message_admission` reads it only
            // inside the `RejectedBeforeEffect` arm of its classification
            // match, while `AdmittedEffectPossible` maps unconditionally to
            // `Injected` / `EffectPossible`. Passing `Requeue` alongside an
            // effect-possible classification therefore cannot requeue an
            // attempt that may have reached the provider — it is the least
            // destructive filler for a non-optional parameter.
            record_admission(
                store,
                &fence,
                &admission,
                NoEffectDisposition::Requeue,
                authority_id,
            )
            .await;
            tracing::info!(
                target: "agent_coordination",
                session_id = %session_id,
                message_id = %message_id,
                attempt_number = fence.attempt_number,
                "delivered an agent message as this session's next turn"
            );
            IdleBoundaryDelivery::Dispatched
        }
        Ok(AdmittedMessageTurnOutcome::RejectedBeforeEffect {
            settlement,
            error_class,
        }) => {
            settle_pre_effect(settlement, store, event_bus, error_class).await;
            record_admission(
                store,
                &fence,
                &rejected_admission(grant, &fence, error_class),
                NoEffectDisposition::Requeue,
                authority_id,
            )
            .await;
            IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "provider proved the delivery produced no effect",
            }
        }
        Ok(AdmittedMessageTurnOutcome::Unsupported { settlement }) => {
            settle_pre_effect(
                settlement,
                store,
                event_bus,
                "agent_message_provider_unsupported",
            )
            .await;
            let admission = BoundaryAdmissionV1 {
                provider_kind: grant.request().provider_kind,
                capability_kind: grant.request().provider_kind.capability_kind(),
                delivery_session_id: fence.delivery_session_id,
                session_generation: fence.delivery_session_generation,
                model_invocation_id: fence.delivery_model_invocation_id,
                native_turn_id: None,
                classification: BoundaryClassificationV1::Unsupported,
                provider_error_class: Some("agent_message_provider_unsupported".to_string()),
            };
            // The disposition is INERT on this branch and is passed only
            // because the parameter is not optional. What actually fails the
            // aggregate permanently is the CLASSIFICATION: `record_agent_
            // message_admission` maps `Unsupported` straight to
            // `AgentMessageStateV1::Failed` with its own
            // `AttemptTerminalDispositionV1::Unsupported`, and consults
            // `no_effect_disposition` only in the `RejectedBeforeEffect` arm.
            // `Failed` is passed anyway so that the argument states the honest
            // intent rather than contradicting the outcome. That permanence is
            // right: retrying a provider that structurally has no
            // admitted-message path would loop forever.
            record_admission(
                store,
                &fence,
                &admission,
                NoEffectDisposition::Failed,
                authority_id,
            )
            .await;
            IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                reason: "provider does not support admitted message turns",
            }
        }
        // A daemon-internal fault of UNKNOWN effect. The plan forbids reading
        // no-effect out of a generic error, so this is recorded conservatively
        // as effect-possible and the session goes terminal without a synthetic
        // continuation.
        Err(error) => {
            tracing::warn!(
                target: "agent_coordination",
                session_id = %session_id,
                message_id = %message_id,
                error = %error,
                "agent-message dispatch failed with unknown effect; classifying effect-possible"
            );
            let admission = BoundaryAdmissionV1 {
                provider_kind: grant.request().provider_kind,
                capability_kind: grant.request().provider_kind.capability_kind(),
                delivery_session_id: fence.delivery_session_id,
                session_generation: fence.delivery_session_generation,
                model_invocation_id: fence.delivery_model_invocation_id,
                native_turn_id: None,
                classification: BoundaryClassificationV1::AdmittedEffectPossible,
                provider_error_class: Some("agent_message_dispatch_failed".to_string()),
            };
            record_admission(
                store,
                &fence,
                &admission,
                NoEffectDisposition::Requeue,
                authority_id,
            )
            .await;
            IdleBoundaryDelivery::EffectPossibleTerminal {
                error_class: "agent_message_dispatch_failed",
            }
        }
    }
}

/// The admission value for a PROVED pre-effect rejection.
fn rejected_admission(
    grant: &ArbitrationGrant,
    fence: &MessageAttemptFenceV1,
    error_class: &'static str,
) -> BoundaryAdmissionV1 {
    BoundaryAdmissionV1 {
        provider_kind: grant.request().provider_kind,
        capability_kind: grant.request().provider_kind.capability_kind(),
        delivery_session_id: fence.delivery_session_id,
        session_generation: fence.delivery_session_generation,
        model_invocation_id: fence.delivery_model_invocation_id,
        // Never populated on a rejection: `validate()` refuses a native turn ID
        // on anything but `admitted_effect_possible`.
        native_turn_id: None,
        classification: BoundaryClassificationV1::RejectedBeforeEffect,
        provider_error_class: Some(error_class.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::model_control::call_control::ModelCallSettlementWorker;
    use crate::provider::TurnId;
    use crate::session::agent_message_arbiter::{AgentMessageArbiter, BoundaryDecision};
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::agent_coordination::{AgentSendMessageRequestV1, AttemptStateV1};
    use rsi_common::types::{SessionKind, SessionStatus};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// What a fake provider did when the delivery path called it.
    #[derive(Debug, Default)]
    struct ProviderProbe {
        called: AtomicBool,
        /// Set when the provider could take the store lock at the moment of
        /// dispatch. See `no_store_guard_is_held_across_the_provider_dispatch`.
        store_was_unlocked: AtomicBool,
        /// The attempt row's durable `attempt_state` READ FROM THE DATABASE at
        /// the instant the provider was called.
        ///
        /// Was `attempt_was_already_claimed: AtomicBool` before H21-P2-R5-001.
        /// A bool that only recorded "is it `claimed`?" could not express the
        /// post-fix invariant, which is strictly stronger: the row must be past
        /// `claimed` and specifically at the pre-dispatch marker. Recording the
        /// observed state itself also makes a failure diagnostic — it reports
        /// WHICH state was seen instead of just that a flag was false.
        attempt_state_at_dispatch: std::sync::Mutex<Option<String>>,
        /// The payload the provider was handed.
        delivered_input: std::sync::Mutex<Option<String>>,
    }

    /// A provider that ACCEPTS the admitted turn, standing in for
    /// `CodexAppServerSession` without a live `codex app-server` process.
    ///
    /// It exists only to observe the dispatch instant from the inside. Every
    /// test that needs the fail-closed *default* uses [`default_provider`]
    /// instead, which is a real production type rather than a fake — see the
    /// note there for why that distinction is load-bearing.
    struct FakeProvider {
        probe: Arc<ProviderProbe>,
        store: Arc<Mutex<Store>>,
        message_id: Uuid,
    }

    /// The genuine trait default, exercised through a REAL production type.
    ///
    /// `CliProviderSession` is the production `ProviderSession` wrapper for
    /// every CLI-backed provider — Claude, Codex/Pioneer CLI, Antigravity, Local — and
    /// it overrides neither `start_admitted_message_turn` nor
    /// `supports_multi_turn`. Using it here buys two things a hand-written fake
    /// cannot:
    ///
    /// 1. **The default body actually runs.** A fake that reconstructs
    ///    `Unsupported { settlement }` in its own override would assert only
    ///    that the test agrees with itself; the real `provider.rs` default would
    ///    never execute, and a regression that made it dispatch, or drop the
    ///    settlement, would go unnoticed.
    /// 2. **The ordinary path is covered, not just the new one.** `monitor.rs`
    ///    is live for every session of every provider, and the only thing
    ///    keeping non-AppServer providers away from this module is
    ///    `supports_multi_turn`. Asserting that on the real type — see
    ///    [`the_non_app_server_provider_path_is_fail_closed_and_unreachable`] —
    ///    pins the gate rather than assuming it.
    fn default_provider() -> crate::provider::CliProviderSession {
        let (_tx, rx) = tokio::sync::mpsc::channel::<crate::claude::StreamEvent>(1);
        crate::provider::CliProviderSession::new(rx)
    }

    #[async_trait::async_trait]
    impl ProviderSession for FakeProvider {
        async fn next_event(&mut self) -> Option<crate::claude::StreamEvent> {
            None
        }

        async fn start_turn(&mut self, _config: &TurnConfig) -> crate::error::Result<TurnId> {
            panic!("the delivery path must never call the self-admitting start_turn");
        }

        async fn start_admitted_message_turn(
            &mut self,
            turn: NativeMessageTurnRequest,
            config: &TurnConfig,
        ) -> crate::error::Result<AdmittedMessageTurnOutcome> {
            self.probe.called.store(true, Ordering::SeqCst);
            *self.probe.delivered_input.lock().unwrap() = Some(config.input.clone());

            // INVARIANT 1, observed rather than asserted by review: if the
            // caller were holding the store guard across this await, this
            // `try_lock` would fail. It succeeding is positive evidence that no
            // SQLite transaction encloses this external-effect boundary.
            match self.store.try_lock() {
                Ok(guard) => {
                    self.probe.store_was_unlocked.store(true, Ordering::SeqCst);
                    // ORDERING, observed at the only moment it matters: the
                    // attempt must already be durably past its claim AND past
                    // the pre-dispatch marker before any provider can act, or
                    // the crash-window classification is unsound.
                    let state: Option<String> = guard
                        .conn
                        .query_row(
                            "SELECT attempt_state FROM agent_message_delivery_attempts
                              WHERE message_id=?1",
                            rusqlite::params![self.message_id.to_string()],
                            |row| row.get(0),
                        )
                        .ok();
                    *self.probe.attempt_state_at_dispatch.lock().unwrap() = state;
                }
                Err(_) => {
                    self.probe.store_was_unlocked.store(false, Ordering::SeqCst);
                }
            }

            let (_fence, settlement, _execution) = turn.into_parts();
            // Mirror the real AppServer install, which parks the settlement in
            // `current_turn_call` for the life of the turn. Leaking it here is
            // deliberate and is the deterministic choice: holding it in the
            // fake would settle it by `Drop` at end of test, racing the
            // assertions about invocation state, while settling it explicitly
            // would fabricate an outcome the real provider does not produce at
            // this point.
            std::mem::forget(settlement);
            Ok(AdmittedMessageTurnOutcome::Dispatched {
                native_turn_id: None,
            })
        }

        fn supports_multi_turn(&self) -> bool {
            true
        }
    }

    struct Fixture {
        store: Arc<Mutex<Store>>,
        owner_id: Uuid,
        event_bus: Arc<EventBus>,
        settlements: ModelCallSettlementHandle,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        session_id: Uuid,
        message_id: Uuid,
        _worker: ModelCallSettlementWorker,
    }

    /// One live CodexAppServer session with exactly one queued message
    /// addressed to it, built through the REAL acceptance transaction.
    async fn fixture() -> Fixture {
        let raw = Store::open_in_memory().expect("store");
        // Seed the delivery witness exactly as `SessionManager::new` does, so
        // the test reads the same identity production stamps.
        raw.set_delivery_boot_id(Uuid::new_v4())
            .expect("seed delivery boot id");

        let session_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let mut row = test_session(session_id, std::path::PathBuf::from("/tmp"));
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        row.provider = rsi_common::types::SessionProvider::CodexAppServer;
        raw.insert_session(&row).expect("insert delivery session");

        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.session_kind = SessionKind::Task;
        owner.status = SessionStatus::Running;
        raw.insert_session(&owner).expect("insert owner session");

        let message_id = raw
            .accept_agent_message(
                owner_id,
                None,
                &AgentSendMessageRequestV1 {
                    target_session_id: session_id,
                    message: "deliver me".to_string(),
                    idempotency_key: "p2-05b-delivery".to_string(),
                    expires_at: None,
                },
            )
            .expect("acceptance succeeds")
            .receipt()
            .message_id;

        let store = Arc::new(Mutex::new(raw));
        let event_bus = Arc::new(EventBus::new(16));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let settlements = worker.handle().expect("settlement handle");

        let mut map = HashMap::new();
        map.insert(session_id, tracked(row));
        let active = Arc::new(RwLock::new(map));

        Fixture {
            store,
            owner_id,
            event_bus,
            settlements,
            active,
            session_id,
            message_id,
            _worker: worker,
        }
    }

    fn tracked(session: rsi_common::types::Session) -> TrackedSession {
        let session_id = session.id;
        let (stop_tx, _stop_rx) = tokio::sync::mpsc::channel(1);
        TrackedSession {
            session,
            spawn_generation: 0,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            process: None,
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            pending_archive: false,
            rotation: crate::session::rotation_coordinator::RotationCoordinator::new(
                session_id, 0, false,
            ),
            live_input_tokens: 0,
            live_output_tokens: 0,
            live_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
            daemon_input_tokens: 0,
            daemon_output_tokens: 0,
            daemon_tokens_at_last_api_update: 0,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            approval_wait_start: None,
            approval_wait_total_ms: 0,
            work_run_start: None,
            work_time_base_ms: 0,
            received_meaningful_output: true,
            exit_code: Some(0),
            retry_attempt: 0,
            max_retries: 0,
            last_event_at: chrono::Utc::now(),
            stall_interrupted: false,
            last_usage_update: None,
            last_mismatch_warn: None,
            last_classified_at: None,
            classification_count: 0,
            last_verdict: None,
        }
    }

    /// A REAL grant from the REAL selection scan, not a hand-built one.
    async fn grant_for(
        fixture: &Fixture,
    ) -> Box<crate::session::agent_message_arbiter::ArbitrationGrant> {
        let arbiter = Arc::new(AgentMessageArbiter::new());
        let guard = fixture.store.lock().await;
        match crate::session::agent_message_arbiter::decide_next_boundary(
            &guard,
            &arbiter,
            fixture.session_id,
            1,
        ) {
            Ok(BoundaryDecision::DeliverMail(grant)) => grant,
            other => panic!("expected a grant from the real scan, got {other:?}"),
        }
    }

    fn aggregate_state(store: &Store, message_id: Uuid) -> String {
        store
            .conn
            .query_row(
                "SELECT state FROM agent_messages WHERE id=?1",
                rusqlite::params![message_id.to_string()],
                |row| row.get(0),
            )
            .expect("aggregate row")
    }

    fn attempt_row(store: &Store, message_id: Uuid) -> Option<(String, String, String)> {
        store
            .conn
            .query_row(
                "SELECT attempt_state, correlation_state, delivery_boot_id
                   FROM agent_message_delivery_attempts WHERE message_id=?1",
                rusqlite::params![message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
    }

    fn invocation_status(store: &Store, invocation_id: Uuid) -> (String, Option<String>) {
        store
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id=?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("invocation row")
    }

    /// The load-bearing ordering AND the load-bearing lock discipline, both
    /// observed at the one instant they can actually be violated.
    ///
    /// These are one test on purpose: both are properties of the same moment —
    /// the provider dispatch — and observing them from inside the provider is
    /// the only place either can be checked against the real control flow
    /// rather than against a reading of it.
    #[tokio::test]
    async fn a_claim_commits_before_any_provider_dispatch() {
        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let probe = Arc::new(ProviderProbe::default());
        let mut provider = FakeProvider {
            probe: Arc::clone(&probe),
            store: Arc::clone(&fixture.store),
            message_id: fixture.message_id,
        };

        let outcome = deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        assert!(
            matches!(outcome, IdleBoundaryDelivery::Dispatched),
            "a healthy provider must dispatch, got {outcome:?}"
        );
        assert!(
            probe.called.load(Ordering::SeqCst),
            "the provider was called"
        );
        assert!(
            probe.store_was_unlocked.load(Ordering::SeqCst),
            "INVARIANT 1: no SQLite guard may be held across the provider dispatch"
        );
        // STRENGTHENED by H21-P2-R5-001, not relaxed. The pre-fix assertion was
        // `attempt_was_already_claimed`, i.e. "the row is at least durably
        // claimed". That is now guaranteed a fortiori AND is no longer
        // sufficient: seeing `claimed` here would mean the pre-dispatch marker
        // did NOT commit before the send, which is exactly the defect this
        // slice closes — a send leaving behind a row that recovery would read
        // as proof of no effect and redeliver.
        //
        // So the observed state must be EXACTLY `dispatching`: not `claimed`
        // (marker missing ⇒ unsound no-effect proof), and not anything later
        // (TX-C evidence cannot precede the send that produces it).
        assert_eq!(
            probe.attempt_state_at_dispatch.lock().unwrap().as_deref(),
            Some(AttemptStateV1::Dispatching.as_str()),
            "the PRE-DISPATCH MARKER must be durable before the provider can \
             act. `claimed` here means the marker did not commit before the \
             send, which makes the crash-window no-effect proof unsound and \
             licenses a double-delivered paid model turn"
        );
    }

    /// The crash-window rule's precondition, stated as an assertion.
    ///
    /// **RENAMED (H21-P2-R5-001).** This test was called
    /// `a_foreign_boot_id_on_an_unrecorded_claimed_attempt_proves_no_effect`,
    /// which named a conclusion R5 WITHDREW as unsound and this body never
    /// asserted. Because the verification manifest cites test names as
    /// evidence, a name that overstates its test is itself a defect: it makes
    /// the manifest claim a proof nothing in the suite performs. The name now
    /// states exactly what the body pins.
    ///
    /// What it pins: the attempt is stamped with the LIVE daemon delivery
    /// identity. A nil or per-`Store` constructor default would make a boot-id
    /// mismatch meaningless, and a hardcoded literal would make a mismatch
    /// unreachable. That precondition is load-bearing for BOTH classifications
    /// the corrected crash-window block draws — the `claimed` no-effect proof
    /// and the `dispatching` `uncertain` answer — but it establishes neither on
    /// its own.
    #[tokio::test]
    async fn a_delivery_attempt_is_stamped_with_the_live_daemon_boot_id() {
        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let live_boot_id = { fixture.store.lock().await.delivery_boot_id() };
        let probe = Arc::new(ProviderProbe::default());
        let mut provider = FakeProvider {
            probe: Arc::clone(&probe),
            store: Arc::clone(&fixture.store),
            message_id: fixture.message_id,
        };

        deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        let guard = fixture.store.lock().await;
        let (_, _, stamped) = attempt_row(&guard, fixture.message_id).expect("attempt row exists");
        assert!(
            !live_boot_id.is_nil(),
            "the delivery witness must never be nil; nil is the one value two \
             incarnations could share"
        );
        assert_eq!(
            stamped,
            live_boot_id.to_string(),
            "the attempt must be stamped with the LIVE daemon delivery identity, \
             not a literal and not a per-Store constructor default — that equality \
             is exactly what makes a MISMATCH a sound no-effect proof for P2-06"
        );
    }

    /// A dispatched message becomes this session's turn and stays correlatable.
    #[tokio::test]
    async fn a_dispatched_message_is_injected_and_stays_correlation_pending() {
        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let probe = Arc::new(ProviderProbe::default());
        let mut provider = FakeProvider {
            probe: Arc::clone(&probe),
            store: Arc::clone(&fixture.store),
            message_id: fixture.message_id,
        };

        deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        let guard = fixture.store.lock().await;
        assert_eq!(aggregate_state(&guard, fixture.message_id), "injected");
        let (attempt_state, correlation, _) =
            attempt_row(&guard, fixture.message_id).expect("attempt row exists");
        assert_eq!(attempt_state, "effect_possible");
        assert_eq!(
            correlation, "correlation_pending",
            "turn-ID correlation is a separate unit; the attempt must stay \
             correlatable rather than being sealed as not_applicable"
        );
    }

    /// The delivered payload is the WRAPPED rendering, never the sender's bytes.
    #[tokio::test]
    async fn the_delivered_payload_is_the_neutralized_envelope_rendering() {
        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let expected = grant.render_payload_for_delivery();
        let probe = Arc::new(ProviderProbe::default());
        let mut provider = FakeProvider {
            probe: Arc::clone(&probe),
            store: Arc::clone(&fixture.store),
            message_id: fixture.message_id,
        };

        deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        let delivered = probe.delivered_input.lock().unwrap().clone();
        let delivered = delivered.expect("the provider received a payload");
        assert_eq!(
            delivered, expected,
            "delivery must route through render_payload_for_delivery / \
             wrap_agent_message, never around it"
        );
        assert!(
            delivered.contains("rsid-daemon-message"),
            "the wrapper is the point of this slice, not an optional decoration"
        );
        assert_ne!(
            delivered, "deliver me",
            "the raw sender payload must never reach a provider unwrapped"
        );
    }

    /// The ORDINARY path — invariant 6, which demands the untouched providers
    /// be tested rather than assumed safe.
    ///
    /// `monitor.rs` is live for every session of every provider, so the claim
    /// that this slice cannot disturb Claude, Codex/Pioneer CLI, Antigravity, Local or
    /// Harness rests entirely on two properties of the REAL `CliProviderSession`
    /// — not of a fake. This asserts both:
    ///
    /// 1. `supports_multi_turn()` is `false`, which is the enclosing condition
    ///    at the monitor's idle boundary. While it holds, no CLI-backed session
    ///    can reach the delivery module at all, and its path stays byte-identical
    ///    to before this slice.
    /// 2. Even if property 1 were ever broken, the trait default is fail-closed:
    ///    it makes NO provider call, consumes no capability, and hands the
    ///    settlement back for explicit settlement. Defence in depth, so a future
    ///    regression in the gate degrades to a failed message rather than to an
    ///    unadmitted send.
    #[tokio::test]
    async fn the_non_app_server_provider_path_is_fail_closed_and_unreachable() {
        let mut provider = default_provider();
        assert!(
            !provider.supports_multi_turn(),
            "the monitor's idle-boundary gate is `supports_multi_turn`; if a \
             CLI-backed provider ever reported true, every non-AppServer session \
             would start reaching the delivery path"
        );

        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let outcome = deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        assert!(
            matches!(
                outcome,
                IdleBoundaryDelivery::FellBackToSyntheticContinuation { .. }
            ),
            "the fail-closed default must fall back to the ordinary continuation, \
             never go terminal, got {outcome:?}"
        );
    }

    /// A provider with no admitted-message path fails the message CLOSED,
    /// settles the invocation explicitly, and never pretends to have sent.
    ///
    /// Driven through the REAL `CliProviderSession` so the genuine
    /// `provider.rs` default body executes — a fake that reconstructed
    /// `Unsupported { settlement }` in its own override would prove only that
    /// the test agrees with itself.
    #[tokio::test]
    async fn an_unsupported_provider_seals_the_attempt_without_any_effect() {
        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let mut provider = default_provider();

        let outcome = deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        assert!(
            matches!(
                outcome,
                IdleBoundaryDelivery::FellBackToSyntheticContinuation { .. }
            ),
            "an unsupported provider must fall back, never go terminal, got {outcome:?}"
        );

        let guard = fixture.store.lock().await;
        assert_eq!(
            aggregate_state(&guard, fixture.message_id),
            "failed",
            "unsupported is permanent: retrying a provider that structurally \
             cannot deliver would loop forever"
        );
        let (attempt_state, _, _) =
            attempt_row(&guard, fixture.message_id).expect("attempt row exists");
        assert_eq!(attempt_state, "terminal");
    }

    /// The top risk named by the design research, pinned.
    ///
    /// On a non-dispatch exit the invocation must be settled EXPLICITLY with a
    /// pre-effect class. If it were left to `Drop`, a provably clean pre-effect
    /// abort would be recorded as `model_call_task_exited` — a wrong error
    /// class on a path that had no effect at all.
    #[tokio::test]
    async fn a_non_dispatch_exit_settles_explicitly_rather_than_by_drop() {
        let fixture = fixture().await;
        let grant = grant_for(&fixture).await;
        let mut provider = default_provider();

        deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &grant,
            &mut provider,
        )
        .await;

        let invocation_id = {
            let guard = fixture.store.lock().await;
            let raw: String = guard
                .conn
                .query_row(
                    "SELECT delivery_model_invocation_id
                       FROM agent_message_delivery_attempts WHERE message_id=?1",
                    rusqlite::params![fixture.message_id.to_string()],
                    |row| row.get(0),
                )
                .expect("attempt row");
            Uuid::parse_str(&raw).expect("invocation uuid")
        };

        let guard = fixture.store.lock().await;
        let (status, error_class) = invocation_status(&guard, invocation_id);
        assert_ne!(
            status, "running",
            "the unused invocation must not be left running"
        );
        assert_eq!(
            error_class.as_deref(),
            Some("agent_message_provider_unsupported"),
            "the settlement must carry the EXPLICIT pre-effect class; \
             `model_call_task_exited` here would mean Drop settled it, which is \
             the failure mode this rule exists to prevent"
        );
    }

    /// Queue one more message for the same delivery session, through the REAL
    /// acceptance transaction.
    async fn queue_another(fixture: &Fixture, key: &str) -> Uuid {
        let guard = fixture.store.lock().await;
        guard
            .accept_agent_message(
                fixture.owner_id,
                None,
                &AgentSendMessageRequestV1 {
                    target_session_id: fixture.session_id,
                    message: "second message".to_string(),
                    idempotency_key: key.to_string(),
                    expires_at: None,
                },
            )
            .expect("second acceptance succeeds")
            .receipt()
            .message_id
    }

    /// `M-H21-P2-P2-04-ARBITER`'s one remaining clause, made true IN FACT.
    ///
    /// The arbiter stage landed everything about that item except *"one
    /// outstanding grant blocks … every other `start_turn`"*, and it could not
    /// land that clause because the boundary took a grant and RELEASED IT
    /// UNDELIVERED. A grant that gates nothing cannot block anything, so
    /// `RootHeldBackReason::GrantOutstanding` was unreachable in fact even
    /// though it was reachable in the registry.
    ///
    /// This test closes it on the real objects: a REAL scan issues a REAL
    /// grant, that grant gates a REAL dispatch that actually reaches a
    /// provider, the monitor HOLDS it exactly as `monitor.rs` now does, and a
    /// second boundary on the same logical root is refused with
    /// `GrantOutstanding`.
    ///
    /// The release leg at the end is what stops this being vacuous. Without
    /// it, a queue that had simply gone empty would produce the same refusal
    /// for an entirely different reason; showing that the SAME boundary grants
    /// immediately once the grant is released proves the hold was the cause.
    #[tokio::test]
    async fn a_grant_held_across_a_real_dispatch_blocks_the_next_boundary_for_that_root() {
        let fixture = fixture().await;
        let arbiter = Arc::new(AgentMessageArbiter::new());

        let first = {
            let guard = fixture.store.lock().await;
            match crate::session::agent_message_arbiter::decide_next_boundary(
                &guard,
                &arbiter,
                fixture.session_id,
                1,
            ) {
                Ok(BoundaryDecision::DeliverMail(grant)) => grant,
                other => panic!("expected a grant from the real scan, got {other:?}"),
            }
        };

        let probe = Arc::new(ProviderProbe::default());
        let mut provider = FakeProvider {
            probe: Arc::clone(&probe),
            store: Arc::clone(&fixture.store),
            message_id: fixture.message_id,
        };
        let outcome = deliver_at_idle_boundary(
            &fixture.store,
            &fixture.event_bus,
            &fixture.settlements,
            &fixture.active,
            fixture.session_id,
            &first,
            &mut provider,
        )
        .await;
        assert!(
            matches!(outcome, IdleBoundaryDelivery::Dispatched),
            "the grant must gate a REAL dispatch, not be released undelivered, got {outcome:?}"
        );
        assert!(
            probe.called.load(Ordering::SeqCst),
            "a grant that never reached a provider would prove nothing about blocking"
        );

        // More mail arrives for the same logical root while the delivered turn
        // is still in flight and the monitor still holds `first`.
        queue_another(&fixture, "p2-05b-delivery-2").await;

        let second = {
            let guard = fixture.store.lock().await;
            crate::session::agent_message_arbiter::decide_next_boundary(
                &guard,
                &arbiter,
                fixture.session_id,
                1,
            )
        };
        assert!(
            matches!(
                second,
                Ok(BoundaryDecision::SyntheticContinuation(
                    crate::session::agent_message_arbiter::BoundaryDeclined::HeldBack(
                        crate::session::agent_message_dispatcher::RootHeldBackReason::GrantOutstanding
                    )
                ))
            ),
            "an outstanding grant held across a live delivered turn must hold its \
             root back with GrantOutstanding, got {second:?}"
        );

        // Non-vacuity: the refusal above must be caused by the HOLD, not by an
        // empty or stuck queue.
        first.release();
        let third = {
            let guard = fixture.store.lock().await;
            crate::session::agent_message_arbiter::decide_next_boundary(
                &guard,
                &arbiter,
                fixture.session_id,
                1,
            )
        };
        assert!(
            matches!(third, Ok(BoundaryDecision::DeliverMail(_))),
            "releasing the grant must free the root again; otherwise the refusal \
             above proved nothing about the grant, got {third:?}"
        );
    }
}
