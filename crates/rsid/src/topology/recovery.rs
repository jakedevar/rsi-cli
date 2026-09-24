//! Budgeted startup recovery and pin retention for durable topology
//! executions (#634, plan §2.4 and §4 rule 3).
//!
//! Recovery never launches directly: it claims the lease for this daemon
//! incarnation and runs the same idempotent `advance` the live driver runs.
//! The attempt row decides between relaunching the same attempt (no admission
//! for its dedup key), settling `lost` (admitted, no session), and adopting
//! the session row, so running the pass twice launches once.

use std::time::{Duration, Instant};

use chrono::{TimeDelta, Utc};
use uuid::Uuid;

use crate::error::Result;
use crate::topology::custody;
use crate::topology::executor::{Executor, NodeEffects, Step};
use crate::topology::store as rows;

/// Failure pins survive this long for forensics (plan §4 rule 3).
pub(crate) const FAILURE_PIN_TTL: TimeDelta = TimeDelta::days(7);
/// Executions cleaned (and discards completed) per recovery pass.
const CLEANUP_LIMIT: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct RecoveryReport {
    /// Executions advanced by this pass, with the step each reached.
    pub(crate) advanced: Vec<(Uuid, Step)>,
    /// More drivable executions exist than the budget allowed.
    pub(crate) deferred: bool,
    /// Two-phase discards whose effects this pass completed.
    pub(crate) discards_completed: usize,
    /// Settlement cleanup steps (pins released, sessions archived).
    pub(crate) cleanup_steps: usize,
}

/// One bounded pass over executions left `accepted|running|cancelling` (or
/// blocked with in-flight attempts) by a previous daemon incarnation.
pub(crate) async fn recover_after_restart<E: NodeEffects>(
    executor: &Executor<E>,
    max_executions: usize,
    time_budget: Duration,
) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    if !executor.effects.enabled() {
        return Ok(report);
    }
    let started = Instant::now();
    let ids = {
        let store = executor.store.lock().await;
        rows::drivable_execution_ids(&store, max_executions.saturating_add(1))?
    };
    report.deferred = ids.len() > max_executions;
    for execution_id in ids.into_iter().take(max_executions) {
        if started.elapsed() > time_budget {
            report.deferred = true;
            break;
        }
        {
            let store = executor.store.lock().await;
            rows::claim_lease(&store, execution_id, executor.boot_id)?;
        }
        match executor.advance(execution_id).await {
            Ok(step) => report.advanced.push((execution_id, step)),
            Err(error) => {
                tracing::warn!(%execution_id, %error, "topology execution recovery deferred");
                report.deferred = true;
            }
        }
    }
    report.discards_completed = executor
        .complete_pending_discards(CLEANUP_LIMIT)
        .await
        .unwrap_or_default();
    report.cleanup_steps = settlement_cleanup(executor, CLEANUP_LIMIT).await;
    Ok(report)
}

/// Settlement cleanup (plan §4 rules 3 and 5): release node pins and
/// archive node sessions of settled executions, success at once, failure after
/// the forensic TTL. Each step is idempotent and recorded, so a crash after
/// the terminal status is replayed here. Preserved-work sandboxes are never
/// archived; only `discard` releases them. Returns steps completed.
pub(crate) async fn settlement_cleanup<E: NodeEffects>(
    executor: &Executor<E>,
    limit: usize,
) -> usize {
    let due = rows::cleanup_due(
        &*executor.store.lock().await,
        Utc::now() - FAILURE_PIN_TTL,
        limit,
    );
    let Ok(due) = due else {
        return 0;
    };
    let mut completed = 0;
    for item in due {
        if item.pins && release_pins(executor, &item).await {
            completed += 1;
        }
        if item.sessions && release_sessions(executor, &item).await {
            completed += 1;
        }
    }
    completed
}

async fn release_pins<E: NodeEffects>(executor: &Executor<E>, item: &rows::CleanupDue) -> bool {
    let Ok(pins) = rows::pins_of_execution(&*executor.store.lock().await, item.execution_id) else {
        return false;
    };
    let mut released = 0;
    for (attempt_id, pin, commit) in &pins {
        // A ref that no longer points at the recorded commit is not ours.
        let gone = !custody::ref_points_at(&item.repo_root, pin, commit)
            || custody::delete_ref(&item.repo_root, pin, commit).is_ok();
        if gone && rows::clear_pin(&*executor.store.lock().await, *attempt_id).is_ok() {
            released += 1;
        }
    }
    if released != pins.len() {
        return false;
    }
    record(executor, item.execution_id, "pins_released", released).await
}

async fn release_sessions<E: NodeEffects>(executor: &Executor<E>, item: &rows::CleanupDue) -> bool {
    let Ok(sessions) = rows::releasable_sessions(&*executor.store.lock().await, item.execution_id)
    else {
        return false;
    };
    let mut released = 0;
    for session_id in &sessions {
        match executor.effects.release_sandbox(*session_id).await {
            Ok(()) => released += 1,
            Err(error) => tracing::warn!(%session_id, %error, "topology node archive deferred"),
        }
    }
    if released != sessions.len() {
        return false;
    }
    record(executor, item.execution_id, "sessions_released", released).await
}

async fn record<E: NodeEffects>(
    executor: &Executor<E>,
    execution_id: uuid::Uuid,
    kind: &str,
    count: usize,
) -> bool {
    let recorded = rows::record_cleanup(&*executor.store.lock().await, execution_id, kind, count);
    match recorded {
        Ok(update) => {
            if let Some(update) = update {
                executor.effects.publish(update);
            }
            true
        }
        Err(error) => {
            tracing::warn!(%execution_id, %error, "topology settlement cleanup not recorded");
            false
        }
    }
}
