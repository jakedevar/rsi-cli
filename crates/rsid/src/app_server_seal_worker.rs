//! The bounded AppServer control worker (C-P2-15).
//!
//! [`AppServerControlPlane`] records lifecycle facts as in-memory latches that
//! never block and never fail for capacity. That makes ingress non-blocking,
//! but an in-memory latch does not survive a daemon restart. This worker is the
//! durable half: on a fixed tick it scans the registered latches and commits
//! each pre-acknowledgement seal to `sealed_live_uncertain` through the Store,
//! **independently of monitor progress**.
//!
//! # Why this module is async and `app_server_control` is not
//!
//! C-P2-15 requires the plane's production surface to stay synchronous: it uses
//! [`std::sync::Mutex`], which makes holding a guard across an `.await` a
//! compile error, and that is what structurally guarantees the stdout reader
//! can never await a bounded mailbox. The Store, by contrast, lives behind a
//! [`tokio::sync::Mutex`] and must be awaited.
//!
//! The two requirements are reconciled by putting the boundary HERE rather than
//! inside the plane. This worker is the only async component, and it touches
//! the plane exclusively through synchronous calls that take and release the
//! plane guard internally, returning owned values. No plane guard is ever alive
//! across an `.await` in this file, and `app_server_control.rs`'s production
//! surface gains no `async fn` and no `.await`.
//!
//! # What this worker does NOT do
//!
//! It settles only the pre-acknowledgement SEAL kinds (`OverflowSeal`,
//! `TimeoutSeal`). Stronger terminal facts — `TurnCompleted`, `TurnFailed`,
//! `ReaderEof`, `ConfirmedProcessDeath` — are turn settlement and belong to the
//! P2-04 arbiter, which is deliberately not built here. Those latches are
//! examined and left dirty for their owner. Because the sweep is a wrapping
//! cursor, they consume a bounded share of each tick and can never starve the
//! seals behind them.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::app_server_control::{
    AppServerControlPlane, AttemptKey, DurableAttemptRegistration, ProviderLifecycleKind,
    QuarantineSealReason, ReconcileOutcome,
};
use crate::store::Store;
use crate::store::agent_coordination::AppServerSealCommitV1;
use rsi_common::agent_coordination::APP_SERVER_CONTROL_TICK_MS;

/// What one tick actually did. Returned for tests and observability; the
/// production loop ignores it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SealWorkerTickStats {
    /// Attempts the quarantine timeout sealed on this tick.
    pub(crate) timed_out: usize,
    /// Latch keys the resumable sweep examined.
    pub(crate) examined: usize,
    /// Dirty latches the sweep claimed.
    pub(crate) claimed: usize,
    /// Seals this tick durably committed for the FIRST time.
    pub(crate) committed: usize,
    /// Seals already durably present — idempotent no-ops.
    pub(crate) already_sealed: usize,
    /// Latches a stronger durable correlation fact superseded.
    pub(crate) superseded: usize,
    /// Seal latches with no durable attempt row. Left dirty.
    pub(crate) unknown: usize,
    /// Non-seal (terminal) latches left for the arbiter.
    pub(crate) deferred_terminal: usize,
    /// Latches whose CAS committed but which a stronger fact had already
    /// replaced in memory, so they deliberately stay dirty.
    pub(crate) reclaimed_stronger: usize,
    /// Store errors this tick. The latch stays dirty and is retried.
    pub(crate) store_errors: usize,
    /// Dirty latches still outstanding after the sweep.
    pub(crate) remaining_dirty: usize,
}

/// The bounded control worker.
pub(crate) struct AppServerSealWorker {
    plane: Arc<AppServerControlPlane>,
    store: Arc<Mutex<Store>>,
    /// Resumable sweep position. Held as a plain field rather than shared
    /// state: the worker is owned by exactly one task.
    cursor: Option<AttemptKey>,
}

impl AppServerSealWorker {
    pub(crate) const fn new(plane: Arc<AppServerControlPlane>, store: Arc<Mutex<Store>>) -> Self {
        Self {
            plane,
            store,
            cursor: None,
        }
    }

    /// Replay every durable live AppServer attempt into the plane, then enable
    /// writer admission (C-P2-15 startup keyset reconciliation).
    ///
    /// An attempt that is durably `sealed_live_uncertain` is registered as
    /// ALREADY sealed, so a seal committed before a crash stays in force and is
    /// never counted or committed a second time after restart.
    ///
    /// # Errors
    ///
    /// Propagates a Store read failure. The caller must leave writer admission
    /// disabled in that case rather than admitting a possibly duplicate turn.
    pub(crate) async fn reconcile_startup(&self) -> crate::error::Result<ReconcileOutcome> {
        let durable = {
            let store = self.store.lock().await;
            store.list_live_app_server_attempts_v1()?
        };
        let registrations: Vec<DurableAttemptRegistration> = durable
            .into_iter()
            .map(|row| DurableAttemptRegistration {
                fence: row.fence,
                correlation: row.correlation,
                provider_turn_id: row.provider_turn_id,
            })
            .collect();
        Ok(self.plane.reconcile_startup(&registrations, Instant::now()))
    }

    /// One bounded pass. At most 32 dirty latches or 2 ms of scanning, then at
    /// most one single-statement CAS per claimed seal latch.
    pub(crate) async fn tick(&mut self) -> SealWorkerTickStats {
        let started_at = Instant::now();
        let mut stats = SealWorkerTickStats {
            timed_out: self.plane.seal_timed_out_attempts(started_at).len(),
            ..SealWorkerTickStats::default()
        };

        // Nothing to do: return without taking the Store lock at all. This is
        // what keeps an idle daemon from doing per-tick database work; the
        // fixed interval is what keeps it from busy-looping.
        if self.plane.dirty_latch_count() == 0 {
            return stats;
        }

        let sweep = self.plane.drain_dirty_latches_from(self.cursor, started_at);
        self.cursor = sweep.next_cursor;
        stats.examined = sweep.examined;
        stats.claimed = sweep.latches.len();
        stats.remaining_dirty = sweep.remaining_dirty;

        // Only pre-acknowledgement seals are this worker's to settle.
        let mut seals = Vec::new();
        for latch in sweep.latches {
            match latch.kind {
                ProviderLifecycleKind::OverflowSeal | ProviderLifecycleKind::TimeoutSeal => {
                    let reason = latch
                        .seal_reason
                        .unwrap_or(QuarantineSealReason::IngressMailboxOverflow);
                    seals.push((latch.key, latch.kind, reason));
                }
                ProviderLifecycleKind::TurnCompleted
                | ProviderLifecycleKind::TurnFailed
                | ProviderLifecycleKind::ReaderEof
                | ProviderLifecycleKind::ConfirmedProcessDeath => {
                    stats.deferred_terminal += 1;
                }
            }
        }
        if seals.is_empty() {
            return stats;
        }

        // One Store acquisition for the whole bounded batch. Each seal is its
        // own single-statement CAS; no transaction spans the batch, and no
        // provider I/O happens anywhere inside this scope.
        let now = chrono::Utc::now();
        let mut outcomes = Vec::with_capacity(seals.len());
        {
            let store = self.store.lock().await;
            for (key, kind, reason) in seals {
                let committed = store.commit_app_server_evidence_seal_v1(
                    key.message_id,
                    key.attempt_number,
                    reason.as_str(),
                    now,
                );
                outcomes.push((key, kind, committed));
            }
        }

        for (key, kind, outcome) in outcomes {
            let settled = match outcome {
                Ok(AppServerSealCommitV1::Committed) => {
                    stats.committed += 1;
                    true
                }
                Ok(AppServerSealCommitV1::AlreadySealed) => {
                    stats.already_sealed += 1;
                    true
                }
                Ok(AppServerSealCommitV1::Superseded) => {
                    // The attempt durably correlated. That is strictly stronger
                    // than a pre-ack seal, and the forward-only V81 trigger
                    // refuses to overwrite it. Retrying forever would be a
                    // busy-loop against a constraint that is RIGHT.
                    stats.superseded += 1;
                    true
                }
                Ok(AppServerSealCommitV1::Unknown) => {
                    // No durable row yet. Keep the latch dirty: C-P2-15 says a
                    // latch clears only when its exact CAS commits. Dropping it
                    // here would silently lose the seal intent.
                    stats.unknown += 1;
                    false
                }
                Err(error) => {
                    warn!(
                        message_id = %key.message_id,
                        attempt_number = key.attempt_number,
                        error = %error,
                        "AppServer seal CAS failed; latch stays dirty and will be retried"
                    );
                    stats.store_errors += 1;
                    false
                }
            };
            if settled && !self.plane.clear_latch(key, kind) {
                // A stronger terminal fact replaced the latch while the CAS was
                // in flight. It stays dirty for its owner, by design.
                stats.reclaimed_stronger += 1;
            }
        }

        debug!(
            committed = stats.committed,
            already_sealed = stats.already_sealed,
            remaining_dirty = stats.remaining_dirty,
            "AppServer control worker tick"
        );
        stats
    }
}

/// Spawn the worker on the frozen 100 ms tick.
///
/// `MissedTickBehavior::Skip` keeps a stalled tick from bursting a backlog of
/// catch-up passes, which would defeat the per-tick budget.
pub(crate) fn spawn_app_server_seal_worker(
    mut worker: AppServerSealWorker,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(APP_SERVER_CONTROL_TICK_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = interval.tick() => {
                    let _ = worker.tick().await;
                }
            }
        }
    })
}
