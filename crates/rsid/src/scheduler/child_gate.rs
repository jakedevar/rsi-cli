//! The child-aware gate in front of a scheduled `Resume` delivery (#794 S3).
//!
//! A program-mode master's ordinary wake is *held* while its children run
//! (the row stays enabled and exact, so the no-idle invariant still holds) and
//! is released once when the keep-alive window elapses. A daemon keep-alive
//! row is retired undelivered when its children have all settled.

use crate::error::Result;
use crate::store::Store;
use crate::store::child_autonomy::is_keepalive_row;
use chrono::{DateTime, Utc};
use rsi_common::types::ScheduledJob;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResumeGate {
    Deliver,
    /// Leave the row untouched; re-evaluated next tick.
    Hold,
    /// A valve row whose children settled: disable it, deliver nothing.
    RetireKeepalive,
}

pub(super) fn resume_gate(
    store: &Store,
    job: &ScheduledJob,
    now: DateTime<Utc>,
) -> Result<ResumeGate> {
    if is_keepalive_row(job) {
        let Some(parent) = job.wake_session_id else {
            return Ok(ResumeGate::Deliver);
        };
        return Ok(if store.running_children_of(parent)?.is_empty() {
            ResumeGate::RetireKeepalive
        } else {
            ResumeGate::Deliver
        });
    }
    let policy = store.child_autonomy_policy();
    Ok(if store.hold_for_job(job, now, &policy)?.is_some() {
        ResumeGate::Hold
    } else {
        ResumeGate::Deliver
    })
}

/// Disable a settled valve row. It is retained (disabled) as the window
/// ledger until the ordinary retention sweep.
pub(super) fn retire_keepalive(store: &Store, job: &ScheduledJob) -> Result<()> {
    store.update_scheduled_job(
        &job.id,
        &crate::store::scheduled_jobs::ScheduledJobUpdate {
            name: None,
            message: None,
            schedule: None,
            enabled: Some(false),
            next_fire_at: None,
        },
    )
}
