//! Issue #3765: a manager seat whose turn ends cleanly stays resumable.
//!
//! A project PM or portfolio/area manager that ends a turn with exit 0 and no
//! enabled wake used to depend on remembering to arm one before every turn
//! end. When the provider exited 0 without a final `result` event (#3029
//! keeps that Interrupted for ordinary sessions), the seat became
//! `Interrupted`, the parent's slot watch fired "slot ended with no
//! successor" and a fresh seat had to be appointed cold.
//!
//! Two daemon rules, both scoped to appointed manager seats (a session for
//! which `Store::session_holds_manager_seat` holds); an ordinary worker keeps
//! today's behaviour:
//! 1. A clean provider exit (code 0, no halt, deploy drain or other recorded
//!    stop source, some real output) settles the seat `Completed`: the idle
//!    manager state every mail path already wakes (a tier message is a
//!    durable resume wake on the seat). A signal, non-zero exit or requested
//!    interrupt stays `Interrupted` / `Failed`.
//! 2. A seat that finalized `Completed` while holding no enabled
//!    resume/`when` wake gets a daemon-armed `health_anomaly` backstop (#2747)
//!    so it is still woken on a slot anomaly. It never arms `fresh`, never
//!    replaces a wake the seat armed, and is re-armed by the next turn end.

use rsi_common::types::{Session, SessionProvider, SessionStatus};
use uuid::Uuid;

use super::types::{TerminalEvidence, TerminalFinalizeDecision};
use crate::store::Store;

/// Name prefix of the daemon-armed backstop; one row per seat session.
pub(crate) const IDLE_BACKSTOP_NAME_PREFIX: &str = "manager-idle-backstop-";

/// The provider exited 0 on its own with real output but no final result
/// event, so [`super::monitor::terminal_decision`] kept it `Interrupted`.
pub(super) fn is_clean_exit_cutoff(
    decision: TerminalFinalizeDecision,
    evidence: &TerminalEvidence,
) -> bool {
    decision.status == SessionStatus::Interrupted
        && evidence.provider_exit_stop
        && !evidence.interrupt_requested
        && !evidence.process_alive
        && evidence.exit_code == Some(0)
        && (evidence.received_meaningful_output || evidence.prior_meaningful_output)
}

/// Rule 1: promote a clean-exit cut-off of a manager seat to `Completed`.
pub(super) async fn settle_clean_exit(
    store: &rsid_core::store_lock::Mutex<Store>,
    session_id: Uuid,
    decision: TerminalFinalizeDecision,
    evidence: &TerminalEvidence,
) -> TerminalFinalizeDecision {
    if !is_clean_exit_cutoff(decision, evidence) {
        return decision;
    }
    match store.lock().await.session_holds_manager_seat(session_id) {
        Ok(true) => {
            tracing::info!(
                %session_id,
                "Manager seat exited 0 without a result event; settling Completed (idle), not Interrupted"
            );
            TerminalFinalizeDecision::completed()
        }
        Ok(false) => decision,
        Err(error) => {
            tracing::warn!(%session_id, %error, "manager seat check failed; keeping the decision");
            decision
        }
    }
}

/// What the lifecycle hook did for a finalized-`Completed` session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleBackstop {
    /// Not an appointed manager seat: nothing to do.
    NotManager,
    /// The seat already holds an enabled resume/`when` wake.
    HoldsWake,
    /// The daemon armed the `health_anomaly` backstop.
    Armed,
}

/// Rule 2: arm the backstop for an idle manager seat that holds no wake.
/// Never fails the finalizer.
pub(crate) async fn guard_idle_manager(
    store: &rsid_core::store_lock::Mutex<Store>,
    session: &Session,
) -> IdleBackstop {
    if session.status != SessionStatus::Completed || session.pending_archive {
        return IdleBackstop::NotManager;
    }
    let store = store.lock().await;
    match store.session_holds_manager_seat(session.id) {
        Ok(true) => {}
        Ok(false) => return IdleBackstop::NotManager,
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "manager idle backstop check failed");
            return IdleBackstop::NotManager;
        }
    }
    match store.session_has_enabled_resume_wake(session.id) {
        Ok(false) => {}
        Ok(true) => return IdleBackstop::HoldsWake,
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "manager idle backstop check failed");
            return IdleBackstop::HoldsWake;
        }
    }
    match arm_backstop(&store, session) {
        Ok(()) => {
            tracing::info!(session_id = %session.id, "Manager seat went idle with no wake; armed a health_anomaly backstop");
            IdleBackstop::Armed
        }
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "manager idle backstop not armed");
            IdleBackstop::NotManager
        }
    }
}

fn arm_backstop(store: &Store, session: &Session) -> crate::error::Result<()> {
    use crate::session::harness::tools::schedule_wake::{WakeWhenRequest, build_wake_when_job};
    let provider: Option<SessionProvider> = Some(session.provider);
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "[rsid-manager-idle] Your last turn ended with no enabled wake, so the daemon \
                  armed this health_anomaly backstop. Read the condition, act, and arm your own \
                  `resume` or `when` wake before every turn end."
            .to_string(),
        name: Some(format!("{IDLE_BACKSTOP_NAME_PREFIX}{}", session.id)),
        predicate: rsi_common::wake_predicate::WakePredicate {
            health_anomaly: Some(rsi_common::health_anomaly::HealthAnomalySpec::default()),
            ..Default::default()
        },
        timeout_seconds: None,
        working_dir: session
            .sandbox_root
            .clone()
            .unwrap_or_else(|| session.working_dir.clone()),
        provider,
        model: session.model.clone(),
        project_id: session.project_id,
        origin_session_id: session.id,
    })
    .map_err(crate::error::DaemonError::InvalidParam)?;
    crate::scheduler::arm_authorized(store, &job, &state, true).map(|_| ())
}
