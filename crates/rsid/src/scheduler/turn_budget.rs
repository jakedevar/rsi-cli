//! Per-seat wake-turn budget for manager notices (Issue #4021).
//!
//! A manager cannot refuse a wake, so a turn-rate cap written in prose does
//! not hold. The operator sets `manager_max_turns_per_hour` (0 = off). Once a
//! seat has started that many wake-driven turns in the last rolling hour, the
//! scheduler keeps a manager-notice delivery whose every wake-worthy notice is
//! deferrable (a lead's `session_state`, a `work` or non-blocked `intent`
//! ledger row) armed instead of firing it. The notices stay readable in the
//! inbox, and the first delivery after a turn ages out of the hour carries all
//! of them in one coalesced turn. A delivery with any urgent notice (mail, an
//! operator answer, a failure, a gate, a queue or satellite outcome, a safety
//! record) is never held; one made past a spent budget is counted as a bypass.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::coordinator_budget::ManagerTurnBudgetV1;
use uuid::Uuid;

use crate::store::Store;

/// What the budget decides for one manager-notice delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnBudgetVerdict {
    Deliver,
    /// Delivered although the budget is spent, because a notice is urgent.
    Bypass,
    /// Keep the watch armed; a later turn carries these notices.
    Hold,
}

#[derive(Default)]
struct SeatCounters {
    /// Watches currently held, so one hold episode counts once, not per tick.
    held_jobs: HashSet<Uuid>,
    held_wakes: u32,
    urgent_bypasses: u32,
}

fn counters() -> &'static Mutex<HashMap<Uuid, SeatCounters>> {
    static COUNTERS: OnceLock<Mutex<HashMap<Uuid, SeatCounters>>> = OnceLock::new();
    COUNTERS.get_or_init(Default::default)
}

fn window_start(now: DateTime<Utc>) -> String {
    (now - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// The decision for a manager watch about to deliver, and the seat it speaks
/// for. Any read failure delivers: the budget may only ever delay a routine
/// wake, never lose one.
pub(crate) fn decide(store: &Store, job_id: Uuid, now: DateTime<Utc>) -> TurnBudgetVerdict {
    let cap = store.manager_turn_cap_per_hour();
    if cap == 0 {
        release_hold(job_id);
        return TurnBudgetVerdict::Deliver;
    }
    let Ok(Some((_, recipient))) = store.harness_manager_watch_route(job_id) else {
        return TurnBudgetVerdict::Deliver;
    };
    let seat = store.manager_lineage_tip(recipient).unwrap_or(recipient);
    let Ok(used) = store.manager_seat_turns_since(seat, &window_start(now)) else {
        return TurnBudgetVerdict::Deliver;
    };
    if used < cap {
        release_hold(job_id);
        return TurnBudgetVerdict::Deliver;
    }
    match store.manager_watch_wake_deferrable(job_id) {
        Ok(Some(true)) => {
            let mut map = counters().lock().unwrap_or_else(|e| e.into_inner());
            let seat_counters = map.entry(seat).or_default();
            if seat_counters.held_jobs.insert(job_id) {
                seat_counters.held_wakes = seat_counters.held_wakes.saturating_add(1);
                tracing::info!(
                    %seat, %job_id, used, cap,
                    "manager turn budget spent: routine notices coalesced into a later turn"
                );
            }
            TurnBudgetVerdict::Hold
        }
        Ok(Some(false)) => {
            release_hold(job_id);
            let mut map = counters().lock().unwrap_or_else(|e| e.into_inner());
            let seat_counters = map.entry(seat).or_default();
            seat_counters.urgent_bypasses = seat_counters.urgent_bypasses.saturating_add(1);
            tracing::info!(
                %seat, %job_id, used, cap,
                "manager turn budget spent: urgent notice delivered past the cap"
            );
            TurnBudgetVerdict::Bypass
        }
        Ok(None) | Err(_) => TurnBudgetVerdict::Deliver,
    }
}

fn release_hold(job_id: Uuid) {
    let mut map = counters().lock().unwrap_or_else(|e| e.into_inner());
    for seat in map.values_mut() {
        seat.held_jobs.remove(&job_id);
    }
}

/// `decide` under the store lock, for the scheduler.
pub(crate) async fn decide_locked(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    job_id: Uuid,
) -> TurnBudgetVerdict {
    let guard = store.lock().await;
    decide(&guard, job_id, Utc::now())
}

/// The seat's turn budget as the catalog and the operator settings page show it.
pub(crate) fn snapshot(store: &Store, seat: Uuid, now: DateTime<Utc>) -> ManagerTurnBudgetV1 {
    let (held_wakes, urgent_bypasses) = counters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&seat)
        .map_or((0, 0), |c| (c.held_wakes, c.urgent_bypasses));
    ManagerTurnBudgetV1 {
        cap_per_hour: store.manager_turn_cap_per_hour(),
        turns_last_hour: store
            .manager_seat_turns_since(seat, &window_start(now))
            .unwrap_or(0),
        held_wakes,
        urgent_bypasses,
    }
}
