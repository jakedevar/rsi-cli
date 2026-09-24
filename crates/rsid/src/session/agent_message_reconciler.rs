//! P2-06c: the periodic agent-message reconciliation worker.
//!
//! # What this module is
//!
//! Everything P2-06a and P2-06b built was inert.
//! [`Store::list_crashed_agent_message_attempts_page_v1`],
//! [`Store::requeue_crashed_agent_message_attempt_v1`],
//! [`Store::mark_crashed_agent_message_attempt_uncertain_v1`] and
//! [`Store::expire_queued_agent_message_v1`] existed, were hardened, were
//! mutation-proven, and had **zero production callers**. This module is the
//! caller: it is what makes crash recovery and expiry actually run.
//!
//! It does exactly two things per pass, in this order:
//!
//! 1. **Crash recovery**, driven to exhaustion by the P2-06b page cursor.
//!    Every live attempt stamped with a FOREIGN `delivery_boot_id` is either
//!    requeued (the narrowed rule proved no effect) or advanced to `uncertain`.
//! 2. **Queued terminal settlement and expiry**, driven off the seam that
//!    already existed:
//!    [`Store::list_dispatchable_agent_messages`] has been classifying durably
//!    expired and terminal-target `queued` rows and holding them back since
//!    P2-04. Expiry takes precedence; terminal tips settle only after the Store
//!    re-resolves their current lineage and proves no exact durable recovery
//!    owner remains under its own write transaction.
//!
//! # The rule this module is shaped around
//!
//! > **No external effect may occur inside a SQLite transaction.**
//!
//! [`reconcile_agent_messages_pass`] is a **pure synchronous `fn`**. It takes
//! `&Store` and is not a future, so it cannot suspend, and every provider
//! effect in this tree is reached only through a future. A reconciler that
//! cannot suspend cannot dispatch, send, or pay for a model turn — the property
//! is structural rather than a convention a later author has to remember.
//! `reconciliation_is_a_pure_synchronous_function` coerces it to a `fn`
//! pointer, so making it suspendable stops the build.
//!
//! The thin wrapper that acquires the store guard,
//! `SessionManager::reconcile_agent_messages_once`, is held to the same rule by
//! `the_store_guard_is_never_held_across_a_suspension_point`, which asserts its
//! body has exactly ONE suspension point — the lock acquisition itself — so
//! nothing can be awaited while the process-wide store guard is held.
//!
//! **That check is a source scan on purpose, and it is not redundant with the
//! type system.** The obvious type-level version of it does not work, and
//! assuming it does is a trap worth naming: `tokio::sync::MutexGuard<'_, T>` is
//! `Send` whenever `T: Send`, unlike `std::sync::MutexGuard` which is never
//! `Send`. `Store` IS `Send` (it is only `!Sync`), so a future that holds the
//! store guard across an await stays `Send` and `tokio::spawn` accepts it
//! happily. An earlier revision of this module asserted the opposite as a
//! compile-time guarantee; a mutation that inserted `yield_now` while holding
//! the guard SURVIVED it, which is how the claim was found to be false.
//!
//! This module also never holds a `Transaction`: it only calls `Store` methods
//! that open and commit their own, and does nothing between them but in-memory
//! bookkeeping.
//!
//! # The narrowed rule is NOT widened here
//!
//! This worker is the narrowed rule's first production consumer, and it
//! consumes the verdict the store already computed rather than re-deriving one:
//!
//! * [`CrashRecoveryVerdictV1::ProvedNoEffectRequeue`] — `claimed` + foreign
//!   boot + no recorded admission. Requeue.
//! * [`CrashRecoveryVerdictV1::Uncertain`] — everything else still live,
//!   `dispatching` above all. **These STRAND, and stranding is CORRECT.** The
//!   provider may already have been paid. There is deliberately NO path in this
//!   module that "cleans up", resolves, or retries a stranded row, and adding
//!   one would reintroduce the defect (H21-P2-R5-001) the whole phase exists to
//!   prevent.
//!
//! **Lease expiry is never proof of no effect.** `agent_messages.expires_at` is
//! the message's ACCEPTANCE deadline; `claim_expires_at` is the delivery LEASE.
//! This module reads neither: the queued half asks the selection scan which
//! rows are durably expired or terminal, and the crash half asks the
//! classifier, whose statement deliberately cannot see the lease.
//!
//! Expiry here is `queued → expired` ONLY. The `claimed → expired` edge belongs
//! exclusively to `record_agent_message_admission`
//! (`NoEffectDisposition::Expired`), which is precondition-guarded on
//! `expires_at`; authoring a second writer for that edge would race that guard.
//!
//! # Failure posture: a reconciliation failure can never be a boot failure
//!
//! Crash recovery runs at daemon start, on the shared path that serves EVERY
//! session of EVERY provider. So the entry point **has no error channel at
//! all** — [`reconcile_agent_messages_pass`] returns
//! [`AgentMessageReconciliationReport`], not `Result`, and a caller therefore
//! cannot write `?`. Per-row and per-page failures are counted into the report
//! and logged loudly; nothing aborts the daemon.
//!
//! That is log-and-continue, chosen over fail-closed **because there is nothing
//! to close**. Fail-closed is the right posture for
//! `start_app_server_seal_worker`, whose startup reconciliation gates writer
//! admission, so failing withholds a capability that could otherwise duplicate
//! a paid turn. Reconciliation gates nothing: it repairs custody the previous
//! incarnation left behind. Withholding it on failure repairs strictly less,
//! and propagating the failure would let one bad row stop the daemon from
//! starting — strictly worse than leaving mail unreconciled for one interval.
//!
//! In particular, P2-06b deliberately made an unpageable cursor key a LOUD
//! `Err` rather than a skip. R9 cleared it as unreachable today (`message_id`
//! carries a uuid CHECK, `attempt_number` an `INTEGER CHECK(>0)`) and handed
//! the WIRING question here. The answer: that `Err` ends the crash drain for
//! this pass, is logged at `error`, is counted, and leaves
//! `crash_recovery_exhausted == false` — so the pass never CLAIMS an exhaustion
//! it did not observe, and the next tick retries. It cannot reach `main`.
//!
//! # Why a detached tick loop rather than an awaited startup gate
//!
//! The two shapes in `main.rs` have very different failure semantics, and the
//! choice is the safety argument, not a convenience:
//!
//! * an **awaited startup gate** (`start_app_server_seal_worker`) blocks boot
//!   on reconciliation and withholds a capability when it fails;
//! * a **detached tick loop** (`reclaim_terminal_build_caches`) runs beside a
//!   live daemon and contains even a panic to its own task.
//!
//! This worker takes the detached shape, with the FIRST tick firing
//! immediately so crash recovery still happens at start. That is sound because
//! **the reconciler provably cannot touch a row this incarnation owns**: the
//! classifier's `WHERE` carries `a.delivery_boot_id != ?1`, so an attempt the
//! running dispatcher is holding right now is invisible to it. Running
//! concurrently with live delivery is therefore not a hazard the gate shape
//! would have removed — and every writer it calls is CAS- or
//! precondition-guarded on top of that.
//!
//! Both crash writers now re-encode that boot fence themselves (R11 LOW-4
//! closed the asymmetry where only
//! [`Store::requeue_crashed_agent_message_attempt_v1`] did). So the classifier
//! excluding live-boot rows, the store mutex serialising the pass, and each
//! writer's own fence are three independent reasons the reconciler cannot touch
//! an attempt this incarnation owns.
//!
//! # Bounding
//!
//! `AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS` (64) bounds one crash page and
//! `AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS` (64) bounds one expiry page. On top
//! of that each drain carries its OWN page and wall-clock budget, so neither
//! half can starve the other and a pass holds the process-wide store mutex for
//! a bounded window.
//!
//! **Termination.** A full page yields `Some(cursor)` even when it happened to
//! be the last, so a drain ends on one final empty page. `next_cursor.is_none()`
//! is the ONLY evidence the population was fully seen, and it is the only thing
//! that sets `*_exhausted`.
//!
//! **Forward progress at any budget.** Both bounds are checked only after a
//! committed unit of durable work, so a drain always advances by at least one
//! row per pass however small the budget is — no budget value can wedge
//! reconciliation into doing nothing forever.
//!
//! **Progress across passes.** A budget-truncated drain does NOT carry its
//! cursor to the next pass; the next pass restarts at `None`. That is correct
//! and deliberate: every row this worker successfully processes drops out of
//! its query's own predicate (a requeue seals the attempt terminal and requeues
//! the aggregate; an uncertainty moves the aggregate out of `('claimed',
//! 'injected')`; an expiry moves the aggregate out of `queued`), so a fresh
//! scan resumes exactly where the last one stopped. Restarting also RETRIES the
//! rows that errored, which carrying the cursor would silently abandon forever.

use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::store::Store;
use crate::store::agent_coordination::{
    AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS, AgentMessageDispatchEligibility, CrashRecoveryVerdictV1,
};

/// How often the reconciliation worker runs.
///
/// Crash recovery does not wait for this: the tick loop's first tick fires
/// immediately at daemon start. This is the steady-state cadence, which bounds
/// how long a durably expired `queued` row sits before it is expired. Message
/// acceptance deadlines are minutes-scale, so a minute is comfortably inside
/// the resolution anyone can observe.
pub const AGENT_MESSAGE_RECONCILE_INTERVAL_SECS: u64 = 60;

/// Pages one drain may read in a single pass.
///
/// Sixty-four-row pages, so this is 1024 rows per drain per pass — far more
/// than any realistic crashed population, while still refusing to scan
/// unboundedly if one ever appears.
pub(crate) const AGENT_MESSAGE_RECONCILE_MAX_PAGES_PER_DRAIN: usize = 16;

/// Wall-clock one drain may spend in a single pass.
///
/// # Why the time bound lives HERE and not in the page query (P2-06b's deferral)
///
/// The plan's contract is "64 rows or 10 ms per transaction", and P2-06b built
/// only the row half, recording its reasoning and deferring the decision to this
/// slice. The decision is: **do not put a time bound inside
/// `list_crashed_agent_message_attempts_page_v1`; put a real one here.**
///
/// One correction to P2-06b's stated reason first. It argued that query has "no
/// per-row I/O amplification"; strictly it is a JOIN from
/// `agent_message_delivery_attempts` to `agent_messages`, so each attempt row
/// does cost one primary-key probe. That is still a MUCH cheaper per-row cost
/// than `list_dispatchable_agent_messages`, whose per-row `resolve_lineage_tip`
/// issues one indexed query per lineage hop. That resolver is iterative and
/// cycle-safe, but deliberately has no silent depth cap: one row therefore costs
/// its finite stored lineage depth, which is the amplification that earns the
/// dispatch scan its `AGENT_MESSAGE_DISPATCH_SCAN_MAX_MILLIS` check between
/// resolved rows.
///
/// The correction does not change the conclusion, for three reasons:
///
/// 1. **Copying the dispatch scan's shape there would be vacuous.** That scan
///    `collect()`s its rows first and then time-checks the per-row tip
///    resolution. The crashed-attempt scan's post-`collect` loop only parses
///    UUIDs and enums — microseconds for a 64-row page — so a check in the same
///    position could never fire. Shipping a bound that cannot fire, and citing
///    it as a bound, is exactly the kind of decorative constraint this campaign
///    keeps out of the manifest.
/// 2. **Bounding the SQL stepping instead would be untestable.** It is
///    expressible (step manually and check the clock), but nothing in a unit
///    test can make SQLite stepping slow deterministically, so the constraint
///    could never be proved non-vacuous by mutation — and it would restructure a
///    hardened, mutation-proven function whose entire point is that its page
///    bound is never silent. Getting the second exhaustion condition subtly
///    wrong resurrects the defect P2-06b existed to remove.
/// 3. **The transaction is not the contended resource — the store mutex is.**
///    That query holds no explicit transaction and takes no write lock at all,
///    and every writer this worker calls is one `BEGIN IMMEDIATE` over a single
///    row. What a long scan actually costs the daemon is the process-wide
///    `Arc<Mutex<Store>>`, which the whole pass holds. So the bound belongs at
///    the pass, where it governs the resource that is really shared.
///
/// # Placement
///
/// Checked BETWEEN transactions and never inside one, and specifically only
/// after a COMMITTED unit of durable work. That placement is what makes a drain
/// advance by at least one row per pass at ANY budget value, so no budget can
/// wedge reconciliation, and what stops a refused or skipped row from consuming
/// the check and starving the rows behind it.
///
/// # What the budget therefore actually bounds (R11 LOW-3(a))
///
/// An earlier revision of this comment said a pass holds the store for "at most
/// roughly twice this plus one in-flight single-row transaction and one page
/// read". **That quantity is wrong, and the reason is the `committed` gate that
/// the paragraph above describes as a feature.** A row that ERRORS — or, in the
/// expiry drain, one that is merely examined and skipped — never reaches the
/// inner check. So within a single page the clock is not consulted at all, and a
/// page is up to `AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS` (64) rows.
///
/// The honest statement of the bound is: **this budget, plus up to one full page
/// of refused single-row transactions, per drain — so roughly twice that for a
/// pass.** What caps it is the UNCONDITIONAL between-page check further down,
/// which every page must pass through regardless of what happened inside it.
///
/// Measured, not argued:
/// `a_persistently_erroring_crash_population_is_still_walked_to_its_end_by_the_cursor`
/// drives 129 refused single-row transactions through ONE pass — far more than
/// "one in-flight transaction". That test deliberately widens its own clock for
/// an unrelated reason it documents, so the number was ALSO measured against
/// this exact 50 ms default: the erroring pass is identical there, 3 pages and
/// 129 refused transactions, observed 5 runs out of 5.
///
/// The pass is still bounded, and the conclusion that the budget belongs at the
/// pass rather than inside the page query is unchanged; only the stated quantity
/// was false.
pub(crate) const AGENT_MESSAGE_RECONCILE_MAX_MILLIS_PER_DRAIN: u64 = 50;

/// What one drain may spend before it yields and waits for the next pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReconciliationPassBudget {
    pub max_pages_per_drain: usize,
    pub max_millis_per_drain: u64,
}

impl Default for ReconciliationPassBudget {
    fn default() -> Self {
        Self {
            max_pages_per_drain: AGENT_MESSAGE_RECONCILE_MAX_PAGES_PER_DRAIN,
            max_millis_per_drain: AGENT_MESSAGE_RECONCILE_MAX_MILLIS_PER_DRAIN,
        }
    }
}

/// What one reconciliation pass did.
///
/// Every field is reported rather than logged-and-forgotten so a stuck or
/// truncated pass is diagnosable without re-running it, and so a later stage
/// (the dispatcher tick) can gate on `fully_reconciled()` if it ever needs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentMessageReconciliationReport {
    /// Crash pages read this pass.
    pub crash_pages: usize,
    /// Attempts requeued under the narrowed rule (`proved_no_effect_requeue`).
    pub requeued: usize,
    /// Attempts left STRANDED as `uncertain`. Not a failure — the correct
    /// answer for a row whose provider may already have been paid.
    pub stranded_uncertain: usize,
    /// Queued dispatch-scan pages read this pass. The field retains its original
    /// name because terminal settlement extends the same bounded drain.
    pub expiry_pages: usize,
    /// `queued → expired` edges authored this pass.
    pub expired: usize,
    /// `queued → failed` terminal-before-delivery edges authored this pass.
    pub terminal_failed: usize,
    /// Page reads and row writes that returned `Err`. Never fatal.
    pub errors: usize,
    /// `true` only when the crash drain OBSERVED `next_cursor: None`.
    pub crash_recovery_exhausted: bool,
    /// `true` only when the shared queued expiry/terminal drain OBSERVED
    /// `next_cursor: None`.
    pub expiry_exhausted: bool,
    /// A drain stopped on its wall-clock budget rather than on exhaustion.
    pub stopped_by_time_budget: bool,
    /// A drain stopped on its page budget rather than on exhaustion.
    pub stopped_by_page_budget: bool,
}

impl AgentMessageReconciliationReport {
    /// Whether this pass may be claimed to have reconciled everything.
    ///
    /// Deliberately requires `errors == 0` as well as both exhaustion flags: a
    /// drain that walked the whole population while every write failed HAS seen
    /// everything and has fixed nothing, and reporting that as complete is the
    /// same class of lie as a silent page cap.
    #[must_use]
    pub(crate) fn fully_reconciled(self) -> bool {
        self.crash_recovery_exhausted && self.expiry_exhausted && self.errors == 0
    }

    /// Whether anything durable moved. Used to keep the steady-state tick
    /// quiet: a pass that found nothing is the normal case and should not
    /// produce a log line every minute.
    #[must_use]
    pub(crate) fn did_work(self) -> bool {
        self.requeued > 0
            || self.stranded_uncertain > 0
            || self.expired > 0
            || self.terminal_failed > 0
            || self.errors > 0
    }
}

/// Run one bounded reconciliation pass. **Pure, synchronous, effect-free.**
///
/// Synchronous by construction: it takes `&Store`, is not a future, and cannot
/// suspend, so it cannot reach a provider send. That is what makes "no external
/// effect inside a SQLite transaction" structural here rather than conventional
/// — see the module docs.
///
/// Returns a report rather than a `Result` on purpose: this runs at daemon
/// start, and an error channel here would be an error channel into boot.
///
/// `live_boot_id` must be [`Store::delivery_boot_id`] — the identity this
/// incarnation stamps into its own delivery attempts. It serves two roles: the
/// classifier excludes rows carrying it (they belong to a LIVE delivery, not a
/// crash), and it is recorded as the `authority_id` of every edge this worker
/// authors, so the ledger names the exact daemon incarnation that reconciled.
pub(crate) fn reconcile_agent_messages_pass(
    store: &Store,
    live_boot_id: Uuid,
    budget: ReconciliationPassBudget,
) -> AgentMessageReconciliationReport {
    let mut report = AgentMessageReconciliationReport::default();
    reconcile_agent_messages_pass_into(store, live_boot_id, budget, &mut report);
    report
}

/// The pass, writing into a caller-owned report.
///
/// Exists so the panic-catching wrapper can keep whatever durable work the pass
/// had already COMMITTED before it unwound. Splitting it out is the whole reason
/// the accounting survives a panic instead of being discarded with the frame.
fn reconcile_agent_messages_pass_into(
    store: &Store,
    live_boot_id: Uuid,
    budget: ReconciliationPassBudget,
    report: &mut AgentMessageReconciliationReport,
) {
    drain_crash_recovery(store, live_boot_id, budget, report);
    drain_queued_settlement(store, live_boot_id, budget, report);
}

/// The pass, with a panic contained instead of ending reconciliation forever.
///
/// # Why this exists (R11 LOW-2)
///
/// The loop that drives reconciliation is `tokio::spawn`ed and detached, and its
/// `JoinHandle` is dropped. A panic anywhere in a pass therefore kills the task
/// **silently and for the whole process lifetime** — no log line, no retry, and
/// reconciliation simply never runs again until the daemon restarts. That is the
/// "silent permanent degradation" shape, and it is worth removing even though
/// R11 verified it is *currently unreachable*: the production half of this
/// module and all four store writers it calls contain no `unwrap`, `expect`,
/// `panic!`, indexing, or `unreachable!`, arithmetic uses `checked_add`, and
/// integer conversions use `try_from(..).unwrap_or(i64::MAX)`. This is hardening
/// against a future author, not a fix for a live defect.
///
/// # Why the SYNCHRONOUS pass is the thing wrapped
///
/// Because it can be. `reconcile_agent_messages_pass` is a plain `fn`, so it
/// drops straight into `catch_unwind` with none of the `UnwindSafe`/pinning
/// awkwardness a future would bring — the same synchronicity that makes "no
/// external effect inside a SQLite transaction" structural also makes panic
/// containment a three-line affair. Catching *here*, inside the store-guard
/// scope rather than outside it, also means the process-wide guard is released
/// by an ordinary drop on the way out rather than during an unwind.
///
/// `AssertUnwindSafe` is sound here for two specific reasons, not by assumption:
///
/// * [`AgentMessageReconciliationReport`] is a `Copy` struct of independent
///   counters and flags with no cross-field invariant, so a partially-updated
///   one is still meaningful — it is exactly "what had happened so far".
/// * this module never holds a [`crate::store::Transaction`]. Every writer it
///   calls opens and commits its own, and `Transaction`'s `Drop` rolls back, so
///   an unwind cannot leave a half-open transaction visible on the connection.
///
/// # What a caught panic reports
///
/// It is counted as an error, so `fully_reconciled()` is false and no caller can
/// mistake a panicked pass for a clean one; both exhaustion flags are cleared,
/// because a pass that unwound cannot be trusted to have OBSERVED an exhaustion
/// it recorded on the way; and committed work already done is retained, because
/// it is durable whether or not the frame survived. The loop logs and ticks
/// again. A reconciliation panic must never fail the daemon.
pub(crate) fn reconcile_agent_messages_pass_catching_panics(
    store: &Store,
    live_boot_id: Uuid,
    budget: ReconciliationPassBudget,
) -> AgentMessageReconciliationReport {
    catch_reconciliation_panic(
        store,
        live_boot_id,
        budget,
        reconcile_agent_messages_pass_into,
    )
}

/// The containment itself, with the pass injected.
///
/// `pass` is a parameter purely so a test can hand in a body that panics on
/// purpose. Production has exactly one caller and it passes
/// [`reconcile_agent_messages_pass_into`]; there is no test-only hook, no
/// failpoint, and no `#[cfg(test)]` branch in the production path.
fn catch_reconciliation_panic(
    store: &Store,
    live_boot_id: Uuid,
    budget: ReconciliationPassBudget,
    pass: fn(&Store, Uuid, ReconciliationPassBudget, &mut AgentMessageReconciliationReport),
) -> AgentMessageReconciliationReport {
    let mut report = AgentMessageReconciliationReport::default();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pass(store, live_boot_id, budget, &mut report);
    }));

    if let Err(payload) = outcome {
        let panic_message = if let Some(message) = payload.downcast_ref::<&'static str>() {
            (*message).to_string()
        } else if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else {
            "<non-string panic payload>".to_string()
        };

        report.errors += 1;
        // A pass that unwound may have set an exhaustion flag and then died
        // before the drain it belonged to finished. Never carry that claim out.
        report.crash_recovery_exhausted = false;
        report.expiry_exhausted = false;

        tracing::error!(
            target: "agent_coordination",
            panic_message = %panic_message,
            live_boot_id = %live_boot_id,
            requeued = report.requeued,
            stranded_uncertain = report.stranded_uncertain,
            expired = report.expired,
            terminal_failed = report.terminal_failed,
            "agent-message reconciliation PANICKED; the pass is contained and the \
             worker will retry on its next tick. This is a BUG in reconciliation — \
             the pass is supposed to be incapable of panicking — but it must never \
             take reconciliation down for the lifetime of the process"
        );
    }

    report
}

/// The crash-recovery half: drive P2-06b's page cursor to exhaustion.
fn drain_crash_recovery(
    store: &Store,
    live_boot_id: Uuid,
    budget: ReconciliationPassBudget,
    report: &mut AgentMessageReconciliationReport,
) {
    let started = Instant::now();
    let deadline = Duration::from_millis(budget.max_millis_per_drain);
    let mut after = None;

    loop {
        let page = match store.list_crashed_agent_message_attempts_page_v1(live_boot_id, after) {
            Ok(page) => page,
            Err(error) => {
                // No page means no cursor, so this drain cannot advance. End it
                // WITHOUT setting exhausted: the next tick retries from the
                // start, and nothing downstream may believe recovery finished.
                tracing::error!(
                    target: "agent_coordination",
                    error = %error,
                    live_boot_id = %live_boot_id,
                    "agent-message crash recovery could not read its page; \
                     reconciliation is INCOMPLETE for this pass and will retry"
                );
                report.errors += 1;
                return;
            }
        };
        report.crash_pages += 1;

        for attempt in &page.attempts {
            let outcome = match attempt.verdict {
                // The narrowed rule, and only the narrowed rule.
                CrashRecoveryVerdictV1::ProvedNoEffectRequeue => store
                    .requeue_crashed_agent_message_attempt_v1(
                        attempt.message_id,
                        attempt.attempt_number,
                        live_boot_id,
                        live_boot_id,
                    )
                    .map(|()| true),
                // Everything else STRANDS with retained custody. This is not a
                // fallback and not a cleanup: a `dispatching` row may already
                // have been paid for, and requeueing it is forbidden.
                CrashRecoveryVerdictV1::Uncertain => store
                    .mark_crashed_agent_message_attempt_uncertain_v1(
                        attempt.message_id,
                        attempt.attempt_number,
                        attempt.aggregate_state,
                        live_boot_id,
                        live_boot_id,
                    )
                    .map(|()| false),
            };

            let committed = outcome.is_ok();
            match outcome {
                Ok(true) => {
                    report.requeued += 1;
                    tracing::info!(
                        target: "agent_coordination",
                        message_id = %attempt.message_id,
                        attempt_number = attempt.attempt_number,
                        "crash recovery requeued an attempt that provably never dispatched"
                    );
                }
                Ok(false) => {
                    report.stranded_uncertain += 1;
                    tracing::warn!(
                        target: "agent_coordination",
                        message_id = %attempt.message_id,
                        attempt_number = attempt.attempt_number,
                        attempt_state = attempt.attempt_state.as_str(),
                        "crash recovery STRANDED an attempt as uncertain; its provider \
                         effect cannot be ruled out and it must not be redelivered"
                    );
                }
                Err(error) => {
                    // A refusal from a CHECK, a trigger, or an `exactly_one_row`
                    // guard is evidence THIS WORKER is wrong about the row, never
                    // something to route around. Count it, leave the row exactly
                    // as it was, and move on: the cursor still advances, so one
                    // bad row cannot starve the rest of the population.
                    report.errors += 1;
                    tracing::error!(
                        target: "agent_coordination",
                        message_id = %attempt.message_id,
                        attempt_number = attempt.attempt_number,
                        verdict = ?attempt.verdict,
                        error = %error,
                        "agent-message crash recovery refused a row; it is left untouched"
                    );
                }
            }

            // The wall-clock bound is checked AFTER a committed unit of durable
            // work, never inside a transaction and never before one. Two
            // properties fall out of that placement, and both are load-bearing:
            //
            //  * a drain ALWAYS advances by at least one row per pass, whatever
            //    the budget, so no budget value can wedge reconciliation; and
            //  * a row that ERRORED does not consume the check, so a
            //    persistently-refused row cannot stop the drain from reaching
            //    the rows behind it — an errored row is the one kind that does
            //    NOT leave the query's predicate.
            //
            // Stopping mid-page is safe because a truncated drain does not
            // carry its cursor forward: the next pass rescans from the start,
            // and every row already processed has dropped out of the predicate.
            if committed && started.elapsed() >= deadline {
                report.stopped_by_time_budget = true;
                return;
            }
        }

        match page.next_cursor {
            Some(next) => after = Some(next),
            None => {
                report.crash_recovery_exhausted = true;
                return;
            }
        }

        // Budgets are checked BETWEEN pages, after a whole page was reconciled.
        if report.crash_pages >= budget.max_pages_per_drain {
            report.stopped_by_page_budget = true;
            return;
        }
        if started.elapsed() >= deadline {
            report.stopped_by_time_budget = true;
            return;
        }
    }
}

/// The queued terminal/expiry half, sharing one bounded keyset walk.
///
/// Driven off [`Store::list_dispatchable_agent_messages`] rather than a new
/// query. Reusing it keeps expiry precedence and delivery-tip classification in
/// one place. Healthy, missing, and recovery-owned terminal rows advance the
/// keyset cursor without consuming the time budget, so they cannot starve
/// expired or finally-terminal rows behind them.
fn drain_queued_settlement(
    store: &Store,
    live_boot_id: Uuid,
    budget: ReconciliationPassBudget,
    report: &mut AgentMessageReconciliationReport,
) {
    let started = Instant::now();
    let deadline = Duration::from_millis(budget.max_millis_per_drain);
    let mut after = None;

    loop {
        let page = match store
            .list_dispatchable_agent_messages(after, AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS)
        {
            Ok(page) => page,
            Err(error) => {
                tracing::error!(
                    target: "agent_coordination",
                    error = %error,
                    "agent-message expiry could not read its page; reconciliation is \
                     INCOMPLETE for this pass and will retry"
                );
                report.errors += 1;
                return;
            }
        };
        report.expiry_pages += 1;

        for message in &page.messages {
            let outcome = match message.eligibility {
                // The selection ladder checks expiry first, so a terminal target
                // with a passed deadline always takes this edge, never failure.
                AgentMessageDispatchEligibility::Expired => store
                    .expire_queued_agent_message_v1(message.message_id, live_boot_id)
                    .map(|()| true)
                    .map_err(|error| (error, None)),
                AgentMessageDispatchEligibility::DeliverySessionNotLive(status) => store
                    .fail_queued_agent_message_terminal_before_delivery_v1(
                        message.message_id,
                        live_boot_id,
                    )
                    .map(|()| false)
                    .map_err(|error| (error, Some(status))),
                AgentMessageDispatchEligibility::Ready
                | AgentMessageDispatchEligibility::DeliverySessionMissing
                | AgentMessageDispatchEligibility::RecoveryPending(_) => continue,
            };

            match outcome {
                Ok(true) => {
                    report.expired += 1;
                    tracing::info!(
                        target: "agent_coordination",
                        message_id = %message.message_id,
                        owner_session_id = %message.owner_session_id,
                        "expiry reconciler expired a queued message past its acceptance deadline"
                    );
                }
                Ok(false) => {
                    report.terminal_failed += 1;
                    tracing::info!(
                        target: "agent_coordination",
                        message_id = %message.message_id,
                        owner_session_id = %message.owner_session_id,
                        delivery_session_id = %message.delivery_session_id,
                        "reconciler failed queued mail whose delivery tip became terminal"
                    );
                }
                Err(error) => {
                    report.errors += 1;
                    tracing::error!(
                        target: "agent_coordination",
                        message_id = %message.message_id,
                        error = %error.0,
                        selected_terminal_status = ?error.1,
                        "agent-message queued settlement was refused; the row is left untouched"
                    );
                    continue;
                }
            }

            // As in the crash drain: checked after a COMMITTED expiry only.
            // Deliberately NOT after a merely-examined row — the vast majority
            // of this scan's rows are healthy `queued` mail that is skipped, and
            // skipped rows do not leave the predicate. Truncating on one of them
            // would re-examine the same head every pass and never reach the
            // expired row sitting behind it.
            if started.elapsed() >= deadline {
                report.stopped_by_time_budget = true;
                return;
            }
        }

        match page.next_cursor {
            Some(next) => after = Some(next),
            None => {
                report.expiry_exhausted = true;
                return;
            }
        }

        // Budgets are checked BETWEEN pages, after a whole page was examined.
        if report.expiry_pages >= budget.max_pages_per_drain {
            report.stopped_by_page_budget = true;
            return;
        }
        if started.elapsed() >= deadline {
            report.stopped_by_time_budget = true;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structural half of "no external effect inside a transaction".**
    ///
    /// Coercing to a `fn` pointer is the compile-time assertion: if
    /// [`reconcile_agent_messages_pass`] were ever made suspendable, or grew a
    /// provider / channel / `SessionManager` parameter, this line would stop
    /// compiling. Every provider effect in this tree is reached only through a
    /// future, so a function that cannot suspend cannot pay for a model turn —
    /// which is the property, held structurally rather than by inspection.
    ///
    /// The second assertion is that it returns a REPORT and not a `Result`:
    /// that is what makes "a failing reconciliation pass cannot fail the
    /// daemon's boot" unwriteable-around rather than a convention, because a
    /// caller has no `?` to write.
    #[test]
    fn reconciliation_is_a_pure_synchronous_function() {
        let pass: fn(&Store, Uuid, ReconciliationPassBudget) -> AgentMessageReconciliationReport =
            reconcile_agent_messages_pass;

        let store = Store::open_in_memory().expect("open V82 store");
        let report = pass(&store, Uuid::new_v4(), ReconciliationPassBudget::default());

        assert!(
            report.fully_reconciled(),
            "an empty database reconciles completely in one pass"
        );
        assert!(!report.did_work());
    }

    /// The source-level half of the same property.
    ///
    /// The `fn`-pointer coercion above proves this function cannot suspend, but
    /// a determined author could still reach a runtime from synchronous code.
    /// This closes that by asserting the module contains no suspension point,
    /// no runtime re-entry, and no reference to the delivery path at all.
    ///
    /// The needles are assembled with `concat!` so this test's own source does
    /// not contain the strings it forbids.
    #[test]
    fn the_reconciler_cannot_reach_a_provider_effect_at_all() {
        let source = include_str!("agent_message_reconciler.rs");
        for needle in [
            concat!(".", "await"),
            concat!("async", " fn"),
            concat!("block", "_on"),
            concat!("spawn", "_blocking"),
            concat!("start", "_turn"),
            concat!("deliver_at_idle", "_boundary"),
            concat!("Provider", "Session"),
        ] {
            assert!(
                !source.contains(needle),
                "the reconciliation worker must never be able to reach a provider effect, \
                 and `{needle}` is a way in; a reconciler that sends has redelivered a \
                 paid model turn"
            );
        }
    }

    /// **The store guard is never held across a suspension point.**
    ///
    /// The pass itself cannot suspend — it is a plain synchronous `fn`. The
    /// only place a suspension point could be introduced while the process-wide
    /// store guard is held is the four-line wrapper that acquires it, so that
    /// wrapper's body is asserted to contain exactly ONE: the lock acquisition.
    ///
    /// This is deliberately a source scan and NOT a `Send` bound. The type-level
    /// version of this check does not work and quietly passes:
    /// `tokio::sync::MutexGuard<'_, T>` is `Send` whenever `T: Send`, and
    /// `Store` is `Send` (only `!Sync`), so a future holding the guard across an
    /// await remains `Send` and `tokio::spawn` accepts it. A revision of this
    /// module asserted otherwise; a `yield_now` mutation survived it.
    ///
    /// The needles are assembled with `concat!` so this file still contains
    /// none of the strings the test above forbids.
    #[test]
    fn the_store_guard_is_never_held_across_a_suspension_point() {
        let source = include_str!("mod.rs");
        let signature = format!(
            "pub(crate) {} reconcile_agent_messages_once(",
            concat!("async", " fn")
        );
        let start = source
            .find(&signature)
            .expect("the reconciliation entry point moved; this assertion must follow it");
        let body = &source[start..];
        let end = body
            .find("\n    }\n")
            .expect("end of the reconciliation entry point");
        let body = &body[..end];

        assert_eq!(
            body.matches(concat!(".", "await")).count(),
            1,
            "the store guard must be acquired and released without suspending in \
             between. The guard is process-wide, so awaiting while it is held stalls \
             every other daemon store operation — and it is the seam through which a \
             provider effect could be reached from inside reconciliation. Body was:\
             \n{body}"
        );
    }

    /// **R11 LOW-2 — a panic is contained, counted, and does not end
    /// reconciliation for the process lifetime.**
    ///
    /// The loop that drives this is detached and its `JoinHandle` is dropped, so
    /// an escaping panic kills reconciliation silently and permanently. This
    /// asserts the four things containment has to get right:
    ///
    /// 1. the call RETURNS rather than unwinding into the detached task;
    /// 2. the panic is counted as an error, so `fully_reconciled()` is false and
    ///    a panicked pass can never be mistaken for a clean one;
    /// 3. durable work the pass had already COMMITTED is retained — it is on
    ///    disk whether or not the frame survived;
    /// 4. an exhaustion flag the dying pass had set is CLEARED, because a pass
    ///    that unwound cannot be trusted to have observed the `next_cursor:
    ///    None` it recorded.
    ///
    /// The panicking body is injected through the `pass` parameter, so there is
    /// no failpoint and no `#[cfg(test)]` branch in the production path.
    #[test]
    fn a_panicking_pass_is_contained_counted_and_never_ends_reconciliation() {
        let store = Store::open_in_memory().expect("open V82 store");

        // The default hook would print this deliberate panic and make a passing
        // gate look like a failing one. Restored immediately after.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let report = catch_reconciliation_panic(
            &store,
            Uuid::new_v4(),
            ReconciliationPassBudget::default(),
            |_store, _boot, _budget, report| {
                // Durable work that already committed, then a claim of
                // exhaustion, then death.
                report.requeued = 3;
                report.expired = 1;
                report.crash_recovery_exhausted = true;
                report.expiry_exhausted = true;
                panic!("S-R11LOW injected reconciliation panic");
            },
        );
        std::panic::set_hook(previous_hook);

        assert_eq!(
            report.errors, 1,
            "a panicked pass must be counted as a failure, not silently dropped"
        );
        assert_eq!(
            (report.requeued, report.expired),
            (3, 1),
            "work the pass COMMITTED before it panicked is durable and must still \
             be reported; discarding it would under-report edges that really exist"
        );
        assert!(
            !report.crash_recovery_exhausted && !report.expiry_exhausted,
            "a pass that unwound must never carry out an exhaustion claim: it \
             cannot be trusted to have OBSERVED `next_cursor: None`"
        );
        assert!(
            !report.fully_reconciled(),
            "a panicked pass is not a complete pass"
        );
        assert!(
            report.did_work(),
            "the error alone makes this pass loggable"
        );
    }

    /// The containment wrapper must be a pass-through on the ordinary path.
    ///
    /// Guards the other half of LOW-2: it would be easy to add panic
    /// containment and quietly change what a NORMAL pass reports. On an empty
    /// store the wrapper the daemon actually calls must be indistinguishable
    /// from the bare pass.
    #[test]
    fn containment_does_not_change_what_an_ordinary_pass_reports() {
        let store = Store::open_in_memory().expect("open V82 store");
        let boot = Uuid::new_v4();
        let budget = ReconciliationPassBudget::default();

        let bare = reconcile_agent_messages_pass(&store, boot, budget);
        let contained = reconcile_agent_messages_pass_catching_panics(&store, boot, budget);

        assert_eq!(
            bare, contained,
            "wrapping the pass in `catch_unwind` must not change a single field \
             of what a healthy pass reports"
        );
        assert!(contained.fully_reconciled());
        assert!(!contained.did_work());
    }
}
