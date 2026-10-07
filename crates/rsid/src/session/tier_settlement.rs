//! #1266/#1294: settlement of claimed tier mail.
//!
//! A scheduled tier-mail continuation claims its rows (`claimed`) and settles
//! them from its result: launched -> `delivered`, failed before any provider
//! effect -> `failed`, failed after one -> `uncertain` (#945: at most once,
//! never auto-replayed). This module owns that settlement and its retry:
//!
//! - a continuation registers its wake rows as in flight before the claim
//!   ([`TierInFlightGuard`]) and records its outcome before the guard drops;
//! - a settlement write that fails keeps the recorded outcome in the ledger,
//!   and [`SessionManager::resettle_tier_mail`] (the 15-second recovery pass)
//!   writes it later, never delivering again;
//! - a row still `claimed` past [`TIER_CLAIM_UNSETTLED_AFTER`] with neither a
//!   continuation in flight nor a recorded outcome settles `uncertain`.
//!
//! A restart loses the ledger; startup reconciliation settles every claimed
//! row `uncertain` before the scheduler runs.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

use super::SessionManager;
use crate::error::{DaemonError, Result};
use crate::store::manager_tier_routing::TierMailSettlement;

/// A claimed row with no continuation in flight and no recorded outcome is
/// settled `uncertain` once its claim is this old.
pub(crate) const TIER_CLAIM_UNSETTLED_AFTER: Duration = Duration::from_secs(120);

/// Most recorded outcomes held for retry. Past it the oldest is dropped; the
/// stale sweep then settles that row `uncertain` (never a replay).
const PENDING_LIMIT: usize = 4096;

#[derive(Debug, Clone)]
struct PendingTierSettlement {
    outcome: TierMailSettlement,
    reason: Option<String>,
    recorded_at: std::time::Instant,
}

/// In-flight claims and recorded-but-unwritten outcomes, per wake row.
#[derive(Debug, Default)]
pub(crate) struct TierSettlementLedger {
    in_flight: HashMap<Uuid, usize>,
    pending: HashMap<Uuid, PendingTierSettlement>,
}

impl TierSettlementLedger {
    fn record(&mut self, jobs: &[Uuid], outcome: TierMailSettlement, reason: Option<&str>) {
        for &job in jobs {
            self.pending.insert(
                job,
                PendingTierSettlement {
                    outcome,
                    reason: reason.map(str::to_string),
                    recorded_at: std::time::Instant::now(),
                },
            );
        }
        while self.pending.len() > PENDING_LIMIT {
            let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, entry)| entry.recorded_at)
                .map(|(job, _)| *job)
            else {
                break;
            };
            self.pending.remove(&oldest);
        }
    }
}

/// Marks a continuation's wake rows in flight until it drops.
pub(crate) struct TierInFlightGuard<'a> {
    ledger: &'a Mutex<TierSettlementLedger>,
    jobs: Vec<Uuid>,
}

impl Drop for TierInFlightGuard<'_> {
    fn drop(&mut self) {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in &self.jobs {
            if let Some(count) = ledger.in_flight.get_mut(job) {
                *count -= 1;
                if *count == 0 {
                    ledger.in_flight.remove(job);
                }
            }
        }
    }
}

/// Test-only: signalled when a recovery pass starts waiting for the Store.
#[cfg(test)]
pub(crate) static RESETTLE_WAITING_FOR_STORE: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

#[cfg(test)]
static SETTLEMENT_WRITE_FAILURES: std::sync::LazyLock<Mutex<std::collections::HashSet<Uuid>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Test-only: the next settlement write of the continuation for `job` fails.
#[cfg(test)]
pub(crate) fn fail_next_tier_settlement_write_for_test(job: Uuid) {
    SETTLEMENT_WRITE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(job);
}

#[cfg(test)]
fn take_settlement_write_failure_for_test(jobs: &[Uuid]) -> bool {
    let mut armed = SETTLEMENT_WRITE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    jobs.iter().any(|job| armed.remove(job))
}

impl SessionManager {
    fn tier_ledger(&self) -> std::sync::MutexGuard<'_, TierSettlementLedger> {
        self.tier_settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register `jobs` as claimed by a continuation in flight.
    pub(super) fn tier_in_flight(&self, jobs: &[Uuid]) -> TierInFlightGuard<'_> {
        let mut ledger = self.tier_ledger();
        for &job in jobs {
            *ledger.in_flight.entry(job).or_insert(0) += 1;
        }
        TierInFlightGuard {
            ledger: &self.tier_settlements,
            jobs: jobs.to_vec(),
        }
    }

    /// Settle the tier mail a continuation claimed from its result. A failed
    /// write keeps the outcome for [`SessionManager::resettle_tier_mail`].
    pub(super) async fn settle_claimed_tier_mail(
        &self,
        jobs: &[Uuid],
        effect_possible: bool,
        result: &Result<impl Sized>,
    ) {
        let (outcome, reason) = match result {
            Ok(_) => (TierMailSettlement::Delivered, None),
            Err(error) if effect_possible => {
                (TierMailSettlement::Uncertain, Some(error.to_string()))
            }
            Err(error) => (TierMailSettlement::Failed, Some(error.to_string())),
        };
        #[cfg(test)]
        let injected = take_settlement_write_failure_for_test(jobs);
        #[cfg(not(test))]
        let injected = false;
        let written = if injected {
            Err(DaemonError::Store(
                "injected tier settlement write failure".into(),
            ))
        } else {
            self.store
                .lock()
                .await
                .settle_tier_messages(jobs, outcome, reason.as_deref())
        };
        if let Err(error) = written {
            tracing::warn!(
                ?jobs,
                outcome = outcome.as_str(),
                %error,
                "tier mail settlement deferred to the recovery pass"
            );
            self.tier_ledger().record(jobs, outcome, reason.as_deref());
        }
    }

    /// #1294, the 15-second recovery pass: write recorded outcomes whose
    /// first write failed, then settle `uncertain` every claimed row older
    /// than [`TIER_CLAIM_UNSETTLED_AFTER`] whose continuation is gone with no
    /// recorded outcome. Never delivers. Returns the number of rows settled.
    ///
    /// # Errors
    /// A persistence error from the stale sweep.
    pub async fn resettle_tier_mail(&self) -> Result<usize> {
        self.resettle_tier_mail_with_clock(chrono::Utc::now).await
    }

    /// [`SessionManager::resettle_tier_mail`] with its clock injected (tests
    /// age claims without sleeping).
    pub(crate) async fn resettle_tier_mail_with_clock(
        &self,
        clock: impl Fn() -> chrono::DateTime<chrono::Utc>,
    ) -> Result<usize> {
        #[cfg(test)]
        RESETTLE_WAITING_FOR_STORE.notify_one();
        let store = self.store.lock().await;
        // #1307: the ledger is read under the Store lock, together with the
        // cutoff and the sweep. A continuation registers in flight before
        // its claim, and the claim needs this lock, so every live claim is
        // in `in_flight`; a recorded outcome enters `pending` before its
        // continuation leaves `in_flight`. Nothing can claim until the
        // sweep ends, so no live claim is swept however late the pass runs.
        let (pending, exclude) = {
            let ledger = self.tier_ledger();
            let pending: Vec<(Uuid, PendingTierSettlement)> = ledger
                .pending
                .iter()
                .map(|(job, entry)| (*job, entry.clone()))
                .collect();
            let exclude: Vec<Uuid> = ledger
                .in_flight
                .keys()
                .chain(ledger.pending.keys())
                .copied()
                .collect();
            (pending, exclude)
        };
        let mut settled = 0;
        for (job, entry) in pending {
            match store.settle_tier_messages(&[job], entry.outcome, entry.reason.as_deref()) {
                Ok(count) => {
                    settled += count;
                    self.tier_ledger().pending.remove(&job);
                }
                Err(error) => tracing::warn!(
                    %job,
                    outcome = entry.outcome.as_str(),
                    %error,
                    "tier mail settlement retry deferred"
                ),
            }
        }
        let cutoff = clock()
            - chrono::Duration::from_std(TIER_CLAIM_UNSETTLED_AFTER)
                .map_err(|error| DaemonError::Store(error.to_string()))?;
        settled += store.settle_stale_claimed_tier_messages(&exclude, cutoff)?;
        Ok(settled)
    }
}
