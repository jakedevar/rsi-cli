//! P2-04: the agent-message dispatcher's bounded selection→grant half.
//!
//! # What this module is, and where it deliberately stops
//!
//! P2-04 splits delivery across two authorities. The **dispatcher** selects
//! eligible mail FIFO by logical root and resolves the current rotation tip.
//! The **monitor that owns `ProviderSession` is the sole idle-boundary
//! arbiter**: it alone decides when a turn boundary exists, grants one exact
//! live generation, and blocks every other `start_turn` while a grant is
//! outstanding. No path may interrupt a provider turn.
//!
//! This module is the first authority only. It ends by producing
//! [`DispatchGrantRequest`]s — a *request* for the arbiter's grant, never a
//! dispatch. Nothing here claims, delivers, or touches a provider.
//!
//! **The arbiter is not built, and that is a structural obstacle rather than an
//! omission.** `SessionManager::monitor_session` is a 22-parameter associated
//! `async fn` (`session/monitor.rs:703`) that owns `Box<dyn ProviderSession>`
//! on its own stack, reached from four production sites — two `tokio::spawn`
//! (`launch.rs:2247`, `lifecycle.rs:1810`) and two inline recursive
//! `Box::pin` re-entries (`rotation.rs:936`, `rotation.rs:2110`). Its only
//! receiver is `stop_rx: mpsc::Receiver<()>`; it has **no inbound channel for
//! external work**. Granting therefore requires threading a new channel through
//! all four sites, which is its own reviewable unit and is left to the next
//! stage rather than improvised here.
//!
//! # Two properties this module makes structural rather than conventional
//!
//! 1. **A provider effect can never occur inside a SQLite transaction.**
//!    [`plan_dispatch_tick`] is a pure synchronous `fn`. It takes no `Store`,
//!    no `Transaction`, and is not `async`, so it cannot hold a transaction and
//!    cannot await anything. The Store read is a separate call whose
//!    transaction commits before planning begins.
//!    `dispatch_tick_planning_is_a_pure_synchronous_function` coerces it to a
//!    `fn` pointer, so making it `async` fails to compile.
//!
//! 2. **The wired delivery path cannot emit an unneutralized payload.**
//!    [`DispatchGrantRequest`]'s payload field is **private**, and the only
//!    method that yields provider-ready text is
//!    [`DispatchGrantRequest::render_payload_for_delivery`], which routes
//!    through `DeliverablePayload::render_for_delivery` — i.e.
//!    `wrap_agent_message`. A grant request cannot hand a caller raw sender
//!    bytes, so envelope forgery is unreachable on this path by construction
//!    rather than by a future author remembering to call the wrapper.
//!
//! # Not wired yet, stated plainly
//!
//! Nothing in this module has a production caller. The dispatcher's tick loop
//! belongs in `main.rs` beside `run_bounded_retry_dispatch`, but a loop
//! installed now would scan the queue on every tick and then discard the result
//! for want of an arbiter to receive it. Adding a real periodic DB scan that
//! provably cannot produce an effect is worse than leaving the seam unwired, so
//! `#![allow(dead_code)]` is retained deliberately and recorded as such in the
//! verification manifest. This is the same posture continuation 6 recorded for
//! the control plane's stateful API.

#![allow(dead_code)]

use std::collections::HashSet;

use rsi_common::agent_coordination::BoundaryProviderKindV1;
use rsi_common::types::SessionStatus;
use uuid::Uuid;

use crate::error::Result;
use crate::store::Store;
use crate::store::agent_coordination::{
    AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS, AgentMessageDispatchCursor,
    AgentMessageDispatchEligibility, DeliverablePayload, DispatchableAgentMessagePage,
};

/// Frozen bound: one tick may request at most this many grants.
///
/// A grant request is a claim on the arbiter's attention, and the arbiter can
/// only ever act on one boundary per Session at a time. Requesting the whole
/// 64-row page at once would queue work the arbiter provably cannot consume,
/// so the tick is bounded well below the scan bound and the remainder is
/// carried by the cursor to the next tick.
pub(crate) const AGENT_MESSAGE_DISPATCH_MAX_GRANTS_PER_TICK: usize = 8;

/// One request for the monitor arbiter's grant, carrying the exact facts the
/// claim transaction will be fenced on.
///
/// Every `expected_*` field is an assertion the claim re-checks under
/// `BEGIN IMMEDIATE`; if any moved in between, the claim returns a typed CAS
/// loss. Holding a request is not permission to deliver.
#[derive(Debug, Clone)]
pub(crate) struct DispatchGrantRequest {
    pub message_id: Uuid,
    pub owner_session_id: Uuid,
    /// The immutable logical root the sender addressed. Progress and queue caps
    /// stay rooted here; only delivery follows the tip.
    pub logical_root_session_id: Uuid,
    /// The resolved rotation tip, never the logical root.
    pub delivery_session_id: Uuid,
    pub expected_state_version: i64,
    pub expected_current_attempt_number: Option<u32>,
    /// The status guard. This — not the generation — is what carries the
    /// rotated-away case; see [`DeliverySessionFacts`]'s note on
    /// `rotation_depth` being constant per row.
    ///
    /// [`DeliverySessionFacts`]: crate::store::agent_coordination::DeliverySessionFacts
    pub expected_status: SessionStatus,
    /// `sessions.rotation_depth`, constant per row: this catches a torn
    /// dispatcher resolution, NOT rotation.
    pub expected_generation: i64,
    pub expected_prior_model_invocation_id: Option<Uuid>,
    pub provider_kind: BoundaryProviderKindV1,
    /// PRIVATE ON PURPOSE. See the module docs: the only way text leaves this
    /// struct is [`Self::render_payload_for_delivery`].
    payload: DeliverablePayload,
}

impl DispatchGrantRequest {
    /// The one provider-ready rendering on the dispatcher path.
    ///
    /// Routes through `DeliverablePayload::render_for_delivery`, i.e.
    /// `wrap_agent_message`. There is deliberately no accessor that returns the
    /// sender's raw bytes.
    #[must_use]
    pub(crate) fn render_payload_for_delivery(&self) -> String {
        self.payload
            .render_for_delivery(self.message_id, self.owner_session_id)
    }

    /// For tests that must prove the payload really is attacker-shaped and was
    /// not sanitized on the way in.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn payload_for_test(&self) -> &DeliverablePayload {
        &self.payload
    }
}

/// Why a logical root produced no grant request this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootHeldBackReason {
    /// This root already has an outstanding grant. One outstanding grant blocks
    /// duplicate and stale dispatcher wakes and every other `start_turn`.
    GrantOutstanding,
    /// The head row is not claimable right now, with the selection pass's own
    /// typed reason.
    HeadNotReady(AgentMessageDispatchEligibility),
    /// The head reported `Ready` but carried no delivery facts. This should be
    /// unreachable — `Ready` implies a resolved tip — so it fails closed rather
    /// than being treated as claimable.
    ReadyWithoutDeliveryFacts,
    /// The tick's frozen grant budget was already spent; this root waits for
    /// the next tick.
    TickGrantBudgetSpent,
}

/// One root that was considered and not granted, with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootHeldBack {
    pub logical_root_session_id: Uuid,
    pub message_id: Uuid,
    /// The resolved rotation tip this head would have been delivered to.
    ///
    /// Carried so the monitor arbiter — which knows its own tip but *not* its
    /// logical root — can tell whether a held-back head was addressed to it.
    /// Matching on `logical_root_session_id` instead would silently compare a
    /// root against a tip and mis-attribute every held-back row for any session
    /// that has rotated.
    pub delivery_session_id: Uuid,
    pub reason: RootHeldBackReason,
}

/// One tick's plan. Entirely advisory until the arbiter grants.
#[derive(Debug)]
pub(crate) struct DispatchTickPlan {
    pub grant_requests: Vec<DispatchGrantRequest>,
    /// Roots considered and declined this tick, each with its reason. Recorded
    /// rather than silently dropped so a stuck queue is diagnosable without
    /// re-running the scan.
    pub held_back: Vec<RootHeldBack>,
    /// `Some` while the queue may hold more rows past this page.
    pub next_cursor: Option<AgentMessageDispatchCursor>,
}

/// Plan one dispatcher tick. **Pure, synchronous, effect-free.**
///
/// Takes no `Store` and no `Transaction` and is not `async`, so by construction
/// it cannot dispatch a provider effect inside a SQLite transaction — the
/// property C-P2-15 and P2-04 both require. The page it consumes was produced
/// by a read whose transaction already committed.
///
/// **FIFO by logical root.** The page arrives in global `(created_at,id)`
/// order, which also orders each individual root, so the first row seen for a
/// root *is* that root's head. Only heads are considered; everything behind a
/// head waits, because one outstanding grant per Session blocks every other
/// `start_turn` and queueing a second request for the same root could only
/// produce a stale grant.
pub(crate) fn plan_dispatch_tick(
    page: DispatchableAgentMessagePage,
    roots_with_outstanding_grant: &HashSet<Uuid>,
) -> DispatchTickPlan {
    let mut roots_seen: HashSet<Uuid> = HashSet::new();
    reduce_dispatchable_page(
        page,
        roots_with_outstanding_grant,
        &mut roots_seen,
        DispatchSelection::Tick,
    )
}

/// How the shared reduction bounds itself (H21-P2-R3-002).
///
/// The reduction below is the single implementation of FIFO-by-root head
/// selection and the eligibility ladder. Both authorities share it so the two
/// can never disagree about which row is a root's head or why one was declined;
/// they differ only in how they bound themselves, which is what this selects.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DispatchSelection {
    /// Dispatcher push tick: at most [`AGENT_MESSAGE_DISPATCH_MAX_GRANTS_PER_TICK`]
    /// grants, with the remainder carried to the next tick by the cursor.
    Tick,
    /// Monitor boundary pull: one delivery tip, decided now.
    ///
    /// The tick budget is deliberately NOT applied. It exists so a push loop
    /// does not queue work the arbiter cannot consume, and it relies on a
    /// *next tick* to carry the remainder. A boundary pull has neither: it
    /// takes at most one grant, for one known tip, and there is no later tick
    /// to inherit what it skipped. Applying the budget here would decline a
    /// monitor whose root merely sorts 9th among ready roots — a permanent
    /// artifact of the other authority's bound rather than a fact about this
    /// monitor's own eligibility.
    BoundaryPull { delivery_session_id: Uuid },
}

/// Reduce one page to its per-root heads under the given selection policy.
///
/// `roots_seen` is threaded by the caller rather than owned here so a
/// multi-page boundary pull keeps one reduction across its pages. This is
/// load-bearing, not stylistic: the page arrives in global `(created_at,id)`
/// order, so a root's head is the first row seen for it *across the whole
/// scan*. A per-page `roots_seen` would make the first row of page N+1 look
/// like a head even when that root's true head was already reduced on page N,
/// and the pull could then grant a message that is not its root's head —
/// breaking FIFO-within-root, which is exactly the property the reduction
/// exists to hold.
pub(crate) fn reduce_dispatchable_page(
    page: DispatchableAgentMessagePage,
    roots_with_outstanding_grant: &HashSet<Uuid>,
    roots_seen: &mut HashSet<Uuid>,
    selection: DispatchSelection,
) -> DispatchTickPlan {
    let mut grant_requests: Vec<DispatchGrantRequest> = Vec::new();
    let mut held_back: Vec<RootHeldBack> = Vec::new();

    for message in page.messages {
        let logical_root_session_id = message.logical_root_session_id;

        // Only the head of each root is a candidate; the rest wait.
        if !roots_seen.insert(logical_root_session_id) {
            continue;
        }

        let hold = |reason: RootHeldBackReason| RootHeldBack {
            logical_root_session_id,
            message_id: message.message_id,
            delivery_session_id: message.delivery_session_id,
            reason,
        };

        if roots_with_outstanding_grant.contains(&logical_root_session_id) {
            held_back.push(hold(RootHeldBackReason::GrantOutstanding));
            continue;
        }

        if message.eligibility != AgentMessageDispatchEligibility::Ready {
            held_back.push(hold(RootHeldBackReason::HeadNotReady(message.eligibility)));
            continue;
        }

        // `Ready` without facts is not reachable through the selection pass;
        // fail closed rather than inventing a fence value.
        let Some(delivery) = message.delivery.clone() else {
            held_back.push(hold(RootHeldBackReason::ReadyWithoutDeliveryFacts));
            continue;
        };

        if matches!(selection, DispatchSelection::Tick)
            && grant_requests.len() >= AGENT_MESSAGE_DISPATCH_MAX_GRANTS_PER_TICK
        {
            held_back.push(hold(RootHeldBackReason::TickGrantBudgetSpent));
            continue;
        }

        let is_this_boundarys_tip = matches!(
            selection,
            DispatchSelection::BoundaryPull { delivery_session_id }
                if delivery_session_id == message.delivery_session_id
        );

        grant_requests.push(DispatchGrantRequest {
            message_id: message.message_id,
            owner_session_id: message.owner_session_id,
            logical_root_session_id,
            delivery_session_id: message.delivery_session_id,
            expected_state_version: message.state_version,
            expected_current_attempt_number: message.current_attempt_number,
            expected_status: delivery.status,
            expected_generation: delivery.generation,
            expected_prior_model_invocation_id: delivery.prior_model_invocation_id,
            provider_kind: delivery.provider_kind,
            payload: message.payload,
        });

        // A boundary pull wants exactly one grant: the `(created_at,id)`-earliest
        // grantable message for its own tip. Once that exists, every later row is
        // by construction later in the same global order, so scanning on could
        // only find a worse candidate. Stopping on a *grant* and not on a hold is
        // deliberate — one delivery tip can serve more than one logical root, so
        // a held-back head for this tip must not stop the scan while a grantable
        // head for another of its roots may still be ahead.
        if is_this_boundarys_tip {
            break;
        }
    }

    DispatchTickPlan {
        grant_requests,
        held_back,
        next_cursor: page.next_cursor,
    }
}

/// Read one bounded page and plan the tick from it.
///
/// The read and the planning are deliberately two statements: the selection
/// scan's `Deferred` transaction opens and commits entirely inside
/// [`Store::list_dispatchable_agent_messages`], and [`plan_dispatch_tick`]
/// receives an owned page with no live handle to the database.
pub(crate) fn plan_next_dispatch_tick(
    store: &Store,
    after: Option<AgentMessageDispatchCursor>,
    roots_with_outstanding_grant: &HashSet<Uuid>,
) -> Result<DispatchTickPlan> {
    let page =
        store.list_dispatchable_agent_messages(after, AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS)?;
    Ok(plan_dispatch_tick(page, roots_with_outstanding_grant))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_coordination::{DeliverySessionFacts, DispatchableAgentMessage};
    use chrono::{DateTime, TimeZone, Utc};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + seconds, 0).unwrap()
    }

    fn ready_message(
        root: Uuid,
        created_at_offset: i64,
        eligibility: AgentMessageDispatchEligibility,
    ) -> DispatchableAgentMessage {
        let delivery_session_id = Uuid::new_v4();
        DispatchableAgentMessage {
            message_id: Uuid::new_v4(),
            owner_session_id: Uuid::new_v4(),
            logical_root_session_id: root,
            created_at: at(created_at_offset),
            expires_at: None,
            state_version: 3,
            current_attempt_number: None,
            payload: DeliverablePayload::new_for_test("hello"),
            delivery_session_id,
            delivery: Some(DeliverySessionFacts {
                status: SessionStatus::Running,
                generation: 0,
                prior_model_invocation_id: None,
                provider_kind: BoundaryProviderKindV1::ClaudeCli,
            }),
            eligibility,
        }
    }

    fn page(messages: Vec<DispatchableAgentMessage>) -> DispatchableAgentMessagePage {
        DispatchableAgentMessagePage {
            messages,
            next_cursor: None,
        }
    }

    #[test]
    fn dispatch_tick_planning_is_a_pure_synchronous_function() {
        // Coercing to a `fn` pointer is the compile-time assertion: if
        // `plan_dispatch_tick` were ever made `async`, or grew a `&Store` or
        // `&Transaction` parameter, this line would stop compiling. That is
        // what makes "no provider effect inside a SQLite transaction"
        // structural rather than a convention a later author must remember.
        let planner: fn(DispatchableAgentMessagePage, &HashSet<Uuid>) -> DispatchTickPlan =
            plan_dispatch_tick;

        let plan = planner(page(Vec::new()), &HashSet::new());
        assert!(plan.grant_requests.is_empty());
        assert!(plan.held_back.is_empty());
    }

    #[test]
    fn only_the_head_of_each_logical_root_is_granted_and_the_rest_wait() {
        let root = Uuid::new_v4();
        let head = ready_message(root, 0, AgentMessageDispatchEligibility::Ready);
        let behind = ready_message(root, 1, AgentMessageDispatchEligibility::Ready);
        let head_id = head.message_id;

        let plan = plan_dispatch_tick(page(vec![head, behind]), &HashSet::new());

        assert_eq!(
            plan.grant_requests.len(),
            1,
            "one outstanding grant per Session means a root may have at most \
             one request in flight"
        );
        assert_eq!(plan.grant_requests[0].message_id, head_id);
        assert!(
            plan.held_back.is_empty(),
            "a row queued behind its own root's head is waiting normally, not \
             held back for a reason worth reporting"
        );
    }

    #[test]
    fn a_root_with_an_outstanding_grant_is_never_granted_again() {
        let root = Uuid::new_v4();
        let message = ready_message(root, 0, AgentMessageDispatchEligibility::Ready);
        let message_id = message.message_id;
        let delivery_session_id = message.delivery_session_id;

        let mut outstanding = HashSet::new();
        outstanding.insert(root);

        let plan = plan_dispatch_tick(page(vec![message]), &outstanding);

        assert!(
            plan.grant_requests.is_empty(),
            "a duplicate or stale dispatcher wake must not produce a second grant"
        );
        assert_eq!(
            plan.held_back,
            vec![RootHeldBack {
                logical_root_session_id: root,
                message_id,
                delivery_session_id,
                reason: RootHeldBackReason::GrantOutstanding,
            }]
        );
    }

    #[test]
    fn only_ready_heads_produce_a_grant_request() {
        // Each non-Ready class is carried through with its own typed reason
        // rather than being collapsed into one "skipped".
        for eligibility in [
            AgentMessageDispatchEligibility::Expired,
            AgentMessageDispatchEligibility::DeliverySessionMissing,
            AgentMessageDispatchEligibility::DeliverySessionNotLive(SessionStatus::Completed),
        ] {
            let root = Uuid::new_v4();
            let plan = plan_dispatch_tick(
                page(vec![ready_message(root, 0, eligibility)]),
                &HashSet::new(),
            );
            assert!(
                plan.grant_requests.is_empty(),
                "{eligibility:?} must never be granted"
            );
            assert_eq!(
                plan.held_back[0].reason,
                RootHeldBackReason::HeadNotReady(eligibility)
            );
        }
    }

    #[test]
    fn a_ready_head_without_delivery_facts_fails_closed() {
        let root = Uuid::new_v4();
        let mut message = ready_message(root, 0, AgentMessageDispatchEligibility::Ready);
        message.delivery = None;

        let plan = plan_dispatch_tick(page(vec![message]), &HashSet::new());

        assert!(plan.grant_requests.is_empty());
        assert_eq!(
            plan.held_back[0].reason,
            RootHeldBackReason::ReadyWithoutDeliveryFacts,
            "a fence value must never be invented for a head that reported \
             Ready without a resolved tip"
        );
    }

    #[test]
    fn the_tick_grant_budget_is_frozen_and_bounded() {
        let messages: Vec<_> = (0..AGENT_MESSAGE_DISPATCH_MAX_GRANTS_PER_TICK + 3)
            .map(|i| {
                ready_message(
                    Uuid::new_v4(),
                    i as i64,
                    AgentMessageDispatchEligibility::Ready,
                )
            })
            .collect();

        let plan = plan_dispatch_tick(page(messages), &HashSet::new());

        assert_eq!(
            plan.grant_requests.len(),
            AGENT_MESSAGE_DISPATCH_MAX_GRANTS_PER_TICK
        );
        assert_eq!(plan.held_back.len(), 3);
        assert!(
            plan.held_back
                .iter()
                .all(|held| held.reason == RootHeldBackReason::TickGrantBudgetSpent),
            "the surplus must be reported as budget-bounded, not as ineligible"
        );
    }

    #[test]
    fn the_grant_request_carries_the_claims_fences_verbatim() {
        let root = Uuid::new_v4();
        let mut message = ready_message(root, 0, AgentMessageDispatchEligibility::Ready);
        message.state_version = 11;
        message.current_attempt_number = Some(2);
        message.delivery = Some(DeliverySessionFacts {
            status: SessionStatus::WaitingApproval,
            generation: 4,
            prior_model_invocation_id: Some(Uuid::new_v4()),
            provider_kind: BoundaryProviderKindV1::CodexCli,
        });
        let expected = message.delivery.clone().unwrap();
        let delivery_session_id = message.delivery_session_id;

        let plan = plan_dispatch_tick(page(vec![message]), &HashSet::new());
        let request = &plan.grant_requests[0];

        // The tip, never the logical root.
        assert_eq!(request.delivery_session_id, delivery_session_id);
        assert_ne!(request.delivery_session_id, request.logical_root_session_id);
        assert_eq!(request.expected_state_version, 11);
        assert_eq!(request.expected_current_attempt_number, Some(2));
        assert_eq!(request.expected_status, expected.status);
        assert_eq!(request.expected_generation, expected.generation);
        assert_eq!(
            request.expected_prior_model_invocation_id,
            expected.prior_model_invocation_id
        );
        assert_eq!(request.provider_kind, expected.provider_kind);
    }
}
