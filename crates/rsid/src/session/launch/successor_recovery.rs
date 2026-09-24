//! Exact recovery of admission interrupted before candidate publication, and
//! bounded settlement of master-successor launches that cannot converge.

use super::*;
use crate::sandbox::git_worktree::{
    RETAINED_SUCCESSOR_ROOT_FOREIGN, RetainedSuccessorRoot, reclaim_retained_successor_root,
};
use crate::store::successor_reservations::{
    AgentSuccessorReservation, UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED,
    UNPUBLISHED_SUCCESSOR_CLEANUP, UnpublishedSuccessorCleanupClaim,
};
use rsi_common::agent_coordination::AgentSuccessorStateV1;

/// Typed class for an `Uncertain` reservation whose bounded relaunch budget
/// is spent. The candidate identity is retired, never replaced in place.
pub(super) const AGENT_SUCCESSOR_RECOVERY_EXHAUSTED: &str = "agent_successor_recovery_exhausted";
/// Relaunches of one `Uncertain` reservation per daemon process.
pub(super) const AGENT_SUCCESSOR_UNCERTAIN_RETRY_LIMIT: u32 = 3;
/// Durable bound across restarts: `updated_at` is written only by state
/// transitions, so it records when the reservation became `Uncertain`.
pub(super) const AGENT_SUCCESSOR_UNCERTAIN_DEADLINE_SECS: i64 = 10 * 60;

/// Launch refusals that retrying the same candidate cannot clear: a full
/// manager ledger, a drifted pre-publication admission (#398), a durable
/// trace the cleanup refuses to adopt, or a foreign root at the candidate path.
const TERMINAL_SUCCESSOR_LAUNCH_CLASSES: &[&str] = &[
    "manager_v2_record_limit",
    "manager_v2_resource_record_limit",
    "agent_successor_cleanup_admission_changed",
    "agent_successor_cleanup_publication_exists",
    RETAINED_SUCCESSOR_ROOT_FOREIGN,
];

fn terminal_successor_launch_class(error: &DaemonError) -> Option<&'static str> {
    let text = error.to_string();
    TERMINAL_SUCCESSOR_LAUNCH_CLASSES
        .iter()
        .copied()
        .find(|class| text.contains(class))
}

#[cfg(test)]
fn uncertain_clock_offsets() -> &'static std::sync::Mutex<std::collections::HashMap<Uuid, i64>> {
    static OFFSETS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Uuid, i64>>> =
        std::sync::OnceLock::new();
    OFFSETS.get_or_init(Default::default)
}

/// Simulate wall time elapsed since a reservation became `Uncertain`
/// (`updated_at` is trigger-protected against backdating).
#[cfg(test)]
pub(super) fn advance_uncertain_clock_for_test(reservation_id: Uuid, seconds: i64) {
    uncertain_clock_offsets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(reservation_id, seconds);
}

#[cfg(test)]
fn uncertain_clock_offset_for_test(reservation_id: Uuid) -> chrono::Duration {
    chrono::Duration::seconds(
        uncertain_clock_offsets()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&reservation_id)
            .copied()
            .unwrap_or(0),
    )
}

fn bounded_reason(prefix: &str, error: &DaemonError) -> String {
    let mut reason = format!("{prefix}: {error}");
    if reason.len() > 256 {
        let mut end = 256;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    reason
}

impl SessionManager {
    /// Called with this candidate's spawn guard and cwd admission held. The
    /// original allocator ran before admission, but its source/custody tuple
    /// was never committed. D00 retains that allocation; a reservation and a
    /// path are not authority to adopt it or mint a replacement generation.
    /// Settle the nonrestorable obligation, never fabricate a new invocation.
    pub(super) async fn settle_unpublished_agent_successor(
        &self,
        reservation: &AgentSuccessorReservation,
    ) -> Result<bool> {
        let id = reservation.candidate_session_id;
        self.refuse_agent_successor_runtime_witness(id).await?;
        let root = self.sandbox_allocator.base_dir().join(id.to_string());
        let invocation = match self
            .store
            .lock()
            .await
            .claim_unpublished_agent_successor_cleanup(reservation, &root)?
        {
            UnpublishedSuccessorCleanupClaim::Missing => {
                return Ok(false); // First admission has not happened yet.
            }
            UnpublishedSuccessorCleanupClaim::SettledDenied => {
                return Err(DaemonError::PolicyDenied(
                    UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED.into(),
                ));
            }
            UnpublishedSuccessorCleanupClaim::Admitted(invocation) => invocation,
        };

        #[cfg(test)]
        pause_controller_candidate_test(
            id,
            ControllerCandidateTestPhase::SuccessorUnpublishedBeforeReap,
        )
        .await;

        // Missing Session is not a process-absence proof. The durable claim
        // retains capacity on inventory/signal failure, task cancellation, or
        // restart. Reuse the checked, bounded exact-session cohort reaper.
        #[cfg(target_os = "linux")]
        tokio::task::spawn_blocking(move || super::super::reaper::reap_orphans_for_session(id))
            .await
            .map_err(|error| {
                DaemonError::Process(format!("successor cleanup reaper join failed: {error}"))
            })??;
        #[cfg(not(target_os = "linux"))]
        return Err(DaemonError::Process(
            "successor cleanup requires exact process-cohort proof".into(),
        ));

        // No consumption/usage estimate is invented, including after reopen.
        // Existing measurements on this invocation remain ledger-owned.
        complete_invocation_by_id(
            &self.store,
            invocation.id,
            InvocationCompletion {
                error_class: Some(UNPUBLISHED_SUCCESSOR_CLEANUP.into()),
                confidence: invocation
                    .usage
                    .estimated_cost_usd
                    .is_none()
                    .then_some(ModelUsageConfidence::Unavailable),
                ..Default::default()
            },
            self.event_bus(),
        )
        .await?;

        #[cfg(test)]
        pause_controller_candidate_test(
            id,
            ControllerCandidateTestPhase::SuccessorUnpublishedAfterSettlement,
        )
        .await;

        self.store
            .lock()
            .await
            .finish_unpublished_agent_successor_cleanup(reservation, &root)?;
        self.event_bus.publish(DaemonEvent::SystemMessage {
            level: "warn".into(),
            message: format!("Successor {} / candidate {id}: interrupted admission and exact process cohort settled; provider was not relaunched; allocation retained for custody recovery", reservation.reservation_id),
        });
        Ok(true)
    }

    /// Refuse while any in-process runtime witness of the candidate exists.
    async fn refuse_agent_successor_runtime_witness(&self, id: Uuid) -> Result<()> {
        if self.active.read().await.contains_key(&id)
            || self.completed.read().await.contains_key(&id)
            || self
                .agent_tokens
                .read()
                .await
                .token_for_session(id)
                .is_some()
        {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_cleanup_runtime_witness_exists".into(),
            ));
        }
        Ok(())
    }

    /// Remove the candidate's own retained pre-admission allocation. Returns
    /// `None` when a durable trace exists, so only the exact unpublished
    /// cleanup may reason about that root. Caller holds the spawn guard.
    async fn reclaim_untraced_agent_successor_root(
        &self,
        reservation: &AgentSuccessorReservation,
    ) -> Result<Option<RetainedSuccessorRoot>> {
        let id = reservation.candidate_session_id;
        let base = self.sandbox_allocator.base_dir().to_path_buf();
        let root = base.join(id.to_string());
        if std::fs::symlink_metadata(&root).is_err() {
            return Ok(Some(RetainedSuccessorRoot::Absent));
        }
        self.refuse_agent_successor_runtime_witness(id).await?;
        if !self
            .store
            .lock()
            .await
            .agent_successor_candidate_untraced(reservation, &root)?
        {
            return Ok(None);
        }
        let working_dir = std::path::PathBuf::from(reservation.inherited_launch()?.working_dir);
        let origin = working_dir.canonicalize().unwrap_or(working_dir);
        let outcome = tokio::task::spawn_blocking(move || {
            reclaim_retained_successor_root(&base, id, &origin)
        })
        .await
        .map_err(|error| {
            DaemonError::Process(format!("successor sandbox reclaim join failed: {error}"))
        })??;
        if outcome == RetainedSuccessorRoot::Reclaimed {
            self.event_bus.publish(DaemonEvent::SystemMessage {
                level: "info".into(),
                message: format!(
                    "Successor {} / candidate {id}: reclaimed its own retained pre-admission sandbox",
                    reservation.reservation_id
                ),
            });
        }
        Ok(Some(outcome))
    }

    /// Launch-path step, under the candidate spawn guard and after the exact
    /// unpublished cleanup found no admission: a root at the candidate path is
    /// either this candidate's untouched allocation (reclaimed, so the
    /// allocator re-cuts it) or foreign (typed refusal, never adopted).
    pub(super) async fn reclaim_retained_agent_successor_root(
        &self,
        reservation: &AgentSuccessorReservation,
    ) -> Result<()> {
        match self
            .reclaim_untraced_agent_successor_root(reservation)
            .await?
        {
            Some(_) => Ok(()),
            None => Err(DaemonError::PolicyDenied(format!(
                "{RETAINED_SUCCESSOR_ROOT_FOREIGN}:durable_trace"
            ))),
        }
    }

    /// Settle one failed launch in the no-row/no-handle reconcile path and
    /// return the original error. Non-retryable classes and an exhausted
    /// `Uncertain` budget settle `Failed` (releasing the Epic lock) instead of
    /// re-driving the same candidate forever; the predecessor stays lead.
    pub(super) async fn settle_agent_successor_launch_failure(
        &self,
        reservation_id: Uuid,
        attempt_id: Uuid,
        error: DaemonError,
    ) -> DaemonError {
        let current = self.store.lock().await.get_agent_successor(reservation_id);
        let Ok(Some(current)) = current else {
            return error;
        };
        if current.state.is_terminal() || current.launch_attempt_id != Some(attempt_id) {
            self.forget_agent_successor_attempts(reservation_id);
            return error;
        }
        let Some((class, reason)) = self
            .successor_failure_terminal_class(&current, &error)
            .await
        else {
            if current.state == AgentSuccessorStateV1::Launching {
                let _ = self.store.lock().await.settle_agent_successor_uncertain(
                    reservation_id,
                    current.state_version,
                    attempt_id,
                    Uuid::new_v4(),
                    "candidate launch returned before establishment could be proven",
                    "agent_successor_launch_uncertain",
                );
            }
            return error;
        };
        let settled = self.store.lock().await.settle_agent_successor_failed(
            reservation_id,
            current.state_version,
            attempt_id,
            Uuid::new_v4(),
            &reason,
            class,
        );
        if let Err(settle_error) = settled {
            tracing::warn!(
                reservation_id = %reservation_id,
                error = %settle_error,
                "Failed to settle non-converging master successor"
            );
            return error;
        }
        self.after_agent_successor_settled_failed(&current, class)
            .await;
        error
    }

    /// `Some((class, reason))` when this failure must retire the candidate:
    /// it is definite (no candidate row or live handle) and either carries a
    /// non-retryable class or exhausts the `Uncertain` relaunch budget.
    async fn successor_failure_terminal_class(
        &self,
        current: &AgentSuccessorReservation,
        error: &DaemonError,
    ) -> Option<(&'static str, String)> {
        let candidate = current.candidate_session_id;
        let candidate_row = self.store.lock().await.get_session(candidate);
        if !matches!(candidate_row, Ok(None)) || self.active.read().await.contains_key(&candidate) {
            return None;
        }
        if let Some(class) = terminal_successor_launch_class(error) {
            return Some((
                class,
                bounded_reason("successor launch refused with a non-retryable class", error),
            ));
        }
        if current.state != AgentSuccessorStateV1::Uncertain {
            return None;
        }
        let attempts = {
            let mut attempts = self
                .successor_uncertain_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let count = attempts.entry(current.reservation_id).or_insert(0);
            *count += 1;
            let count = *count;
            drop(attempts);
            count
        };
        let now = chrono::Utc::now();
        #[cfg(test)]
        let now = now + uncertain_clock_offset_for_test(current.reservation_id);
        let age = now.signed_duration_since(current.updated_at);
        (attempts >= AGENT_SUCCESSOR_UNCERTAIN_RETRY_LIMIT
            || age.num_seconds() >= AGENT_SUCCESSOR_UNCERTAIN_DEADLINE_SECS)
            .then(|| {
                (
                    AGENT_SUCCESSOR_RECOVERY_EXHAUSTED,
                    bounded_reason("uncertain successor relaunch budget exhausted", error),
                )
            })
    }

    async fn after_agent_successor_settled_failed(
        &self,
        settled: &AgentSuccessorReservation,
        class: &'static str,
    ) {
        let reservation_id = settled.reservation_id;
        let candidate = settled.candidate_session_id;
        self.forget_agent_successor_attempts(reservation_id);
        tracing::warn!(
            reservation_id = %reservation_id,
            candidate_session_id = %candidate,
            safe_error_class = class,
            "Master successor settled failed; Epic lead lock released"
        );
        self.event_bus.publish(DaemonEvent::SystemMessage {
            level: "warn".into(),
            message: format!("agent_successor_failed:{reservation_id}:{candidate}:{class}"),
        });
        // The retired candidate id is never launched again. Its own untouched
        // allocation is reclaimed; a foreign or traced root stays for review.
        if class == RETAINED_SUCCESSOR_ROOT_FOREIGN {
            return;
        }
        let _guard = super::super::spawn_single_flight::acquire_spawn_guard(candidate).await;
        if let Err(reclaim_error) = self.reclaim_untraced_agent_successor_root(settled).await {
            tracing::warn!(
                reservation_id = %reservation_id,
                candidate_session_id = %candidate,
                error = %reclaim_error,
                "Retained failed-successor sandbox kept for review"
            );
        }
    }

    fn forget_agent_successor_attempts(&self, reservation_id: Uuid) {
        self.successor_uncertain_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&reservation_id);
    }
}
