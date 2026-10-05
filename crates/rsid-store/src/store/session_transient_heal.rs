//! Failed-transition hook for the transient-failure heal (Issue #1015).
//!
//! The finalizer calls [`heal_failed_session`] once a session has settled
//! `Failed`. Classification, eligibility, scheduling and the manager notice
//! live in `store::transient_heal`; this module owns the child-path bus event.

use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::bus::{DaemonEvent, EventBus};
use crate::store::Store;
use crate::store::transient_heal::{HealSchedule, TransientHealOutcome};

/// Best effort: a heal failure never affects the finalizer.
pub async fn heal_failed_session(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
) {
    let outcome = store
        .lock()
        .await
        .schedule_transient_heal(session_id, Utc::now());
    publish_heal_outcome(event_bus, session_id, outcome);
}

pub fn publish_heal_outcome(
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    outcome: crate::error::Result<TransientHealOutcome>,
) {
    match outcome {
        Ok(
            TransientHealOutcome::Scheduled(schedule) | TransientHealOutcome::Exhausted(schedule),
        ) => {
            tracing::info!(
                %session_id,
                attempt = schedule.attempt,
                reason = %schedule.reason,
                exhausted = schedule.not_before.is_none(),
                "transient failure heal recorded"
            );
            publish_child_heal_event(event_bus, &schedule);
        }
        Ok(
            TransientHealOutcome::NotTransient(_)
            | TransientHealOutcome::Ineligible(_)
            | TransientHealOutcome::Deferred(_),
        ) => {}
        Err(error) => {
            tracing::warn!(%session_id, %error, "transient failure heal scheduling failed");
        }
    }
}

/// The child path: a lead covered by a manager already has a durable inbox
/// notice, so only the remaining sessions get the bus event.
fn publish_child_heal_event(event_bus: &Arc<EventBus>, schedule: &HealSchedule) {
    if schedule.manager_notified {
        return;
    }
    event_bus.publish(DaemonEvent::SessionHealScheduled {
        session_id: schedule.session_id,
        owner_session_id: schedule.owner_session_id,
        attempt: schedule.attempt,
        max_attempts: schedule.max_attempts,
        not_before: schedule
            .not_before
            .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        reason: schedule.reason.clone(),
    });
}
