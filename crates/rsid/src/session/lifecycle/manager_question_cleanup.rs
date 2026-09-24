//! Ownership of a resumed provider whose exact answer publication could not
//! be cleared. This task can stop the provider, but can never resend the answer.

use std::time::Duration;

use super::*;
use crate::model_control::{AdmissionPermit, call_control::ModelCallSettlementHandle};
use crate::session::spawn_single_flight::{ProviderCwdAdmissionGuard, SpawnGuard};
use crate::session::types::ProviderProcess;

pub(super) struct FailedQuestionProcess {
    pub process: ProviderProcess,
    pub completed: CompletedSession,
    pub permit: AdmissionPermit,
    pub spawn_guard: SpawnGuard,
    pub cwd_guard: ProviderCwdAdmissionGuard,
    pub settlements: ModelCallSettlementHandle,
}

impl SessionManager {
    pub(super) async fn retain_failed_manager_question_process(
        &self,
        owner: FailedQuestionProcess,
    ) {
        let store = Arc::clone(&self.store);
        let completed = Arc::clone(&self.completed);
        let event_bus = Arc::clone(&self.event_bus);
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        // One owner per admitted invocation. Existing finite admission limits
        // bound these tasks. A retained owner holds both spawn/cwd guards and
        // the process handle, so no other continuation can start a writer.
        tokio::spawn(async move {
            let FailedQuestionProcess {
                mut process,
                completed: mut cached,
                permit,
                spawn_guard: _spawn_guard,
                cwd_guard: _cwd_guard,
                settlements: _settlements,
            } = owner;
            let session_id = cached.session.id;
            let mut first_tx = Some(first_tx);
            let mut proved_dead = false;
            let mut retry_delay = Duration::from_secs(1);
            loop {
                let outcome = async {
                    // The running ledger row is already durable and charged.
                    // The marker publishes cleanup ownership; boot recovery
                    // also recognizes the earlier answer admission if this
                    // write fails. It does not acknowledge cancellation.
                    store.lock().await.mark_manager_question_cleanup_required(permit.invocation_id())?;
                    if !proved_dead {
                        let kill = tokio::time::timeout(Duration::from_secs(5), process.kill()).await;
                        let alive = process.is_alive();
                        #[cfg(target_os = "linux")]
                        let reap = tokio::task::spawn_blocking(move || {
                            crate::session::reaper::reap_orphans_for_session(session_id)
                        }).await.map_err(|error| DaemonError::Process(format!("question cleanup reaper join failed: {error}")))?;
                        #[cfg(not(target_os = "linux"))]
                        let reap: Result<usize> = Err(DaemonError::Process("question cleanup requires exact process-cohort proof".into()));
                        if alive || reap.is_err() {
                            return Err(DaemonError::Process(format!("question cleanup retained: kill={kill:?}; alive={alive}; checked_reap={reap:?}")));
                        }
                        // is_alive checks the actual child wait status; the
                        // checked reaper additionally proves no stamped orphan
                        // descendants remain. An unsuccessful kill alone is
                        // never interpreted as successful settlement.
                        proved_dead = true;
                    }
                    {
                        let store = store.lock().await;
                        let current = store.get_session(session_id)?
                            .ok_or_else(|| DaemonError::Store("question cleanup session is missing".into()))?;
                        if !matches!(current.status, SessionStatus::Archived | SessionStatus::Deleted) {
                            store.update_session_status(session_id, SessionStatus::Interrupted)?;
                        }
                        cached.session = store.get_session(session_id)?.expect("session checked under Store lock");
                        cached.events = store.load_events(session_id)?;
                    }
                    complete_invocation(&store, &permit, InvocationCompletion {
                        error_class: Some("manager_question_clear_failed".into()),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..Default::default()
                    }, &event_bus).await?;
                    Ok::<(), DaemonError>(())
                }.await;
                match outcome {
                    Ok(()) => {
                        completed.write().await.insert(session_id, cached);
                        if let Some(tx) = first_tx.take() {
                            let _ = tx.send(());
                        }
                        break;
                    }
                    Err(error) => {
                        if let Some(tx) = first_tx.take() {
                            event_bus.publish(DaemonEvent::SystemMessage {
                                level: "error".into(),
                                message: format!("Session {session_id}: exact answer may have been consumed; provider cleanup remains pending: {error}"),
                            });
                            let _ = tx.send(());
                        }
                        #[cfg(test)]
                        crate::session::launch::pause_controller_candidate_test(session_id,
                            crate::session::launch::ControllerCandidateTestPhase::ManagerQuestionCleanupRetained).await;
                        // Retry physical cleanup/settlement only. Neither the
                        // question nor its delivery journal is changed here.
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
                    }
                }
            }
        });
        let _ = first_rx.await;
    }
}

#[cfg(test)]
mod tests;
