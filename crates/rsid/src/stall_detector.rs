//! Background task that periodically scans active sessions for stalls.
//!
//! A session is considered "stalled" when it is in Running or WaitingApproval
//! status and has received no StreamEvent for longer than the configured threshold.
//!
//! When `StallAction::Interrupt` or `StallAction::InterruptAndRetry` is configured,
//! the stall detector will take remediation action for TaskRabbit/Bug sessions.
//!
//! ## Classifier integration (Phase 2, RSI-0XX)
//!
//! When `stall_classifier_tx` is `Some`, the tick branches at the top of
//! the per-session loop: if the session is idle past the per-provider
//! classifier threshold AND not recently classified AND below the
//! lifetime cap, the session ID is signaled to the classifier (sibling
//! task) and the tick `continue`s without taking a static action.
//!
//! **Ordering invariant (R4):** `classifier_idle_secs` MUST stay shorter
//! than `running_secs` so the classifier signal fires before the static
//! stall path. When the classifier returns a `NotifyOnly` verdict and the
//! session stays idle, the static path resumes as the fallback safety
//! net. `continue_session` itself respects `RotationState::PendingInterrupt`
//! / `WritingHandoff`, so the classifier branch may bypass the rotation
//! guard at the signal layer (R3 double-protection).
//!
//! The static reported-set is NOT updated when the classifier branch
//! fires — that way the static path remains the fallback for sessions
//! whose classifier verdict was telemetry-only.

use crate::bus::{DaemonEvent, EventBus};
use crate::reconciliation::{ReconciliationReason, StallAction};
use crate::session::RotationState;
use crate::session::retry_policy;
use crate::session::types::TrackedSession;
use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

/// Configuration for stall detection thresholds.
#[derive(Debug, Clone)]
pub struct StallConfig {
    /// Threshold for Running sessions (seconds).
    pub running_secs: u64,
    /// Threshold for WaitingApproval sessions (seconds).
    pub waiting_secs: u64,
    /// Action for standard (interactive) sessions when stalled.
    pub standard_action: StallAction,
    /// Action for unattended (TaskRabbit/Bug) sessions when stalled.
    pub unattended_action: StallAction,
    /// Per-provider classifier idle threshold for non-Codex sessions.
    /// MUST be < `running_secs` so the classifier branch fires before the
    /// static stall path (R4 ordering invariant).
    pub classifier_idle_secs: u64,
    /// Codex-specific classifier idle threshold. Per-provider because
    /// Codex `item.started` → `item.completed` can span minutes during a
    /// long shell command (R5).
    pub classifier_idle_secs_codex: u64,
    /// Minimum seconds between two classifications of the same session.
    pub classifier_cooldown_secs: u64,
    /// Lifetime cap on classifier verdicts per session.
    pub classifier_max_per_session: u32,
}

impl Default for StallConfig {
    fn default() -> Self {
        Self {
            running_secs: 1800,
            waiting_secs: 3600,
            standard_action: StallAction::Notify,
            unattended_action: StallAction::Notify,
            classifier_idle_secs: 600,
            classifier_idle_secs_codex: 1800,
            classifier_cooldown_secs: 1800,
            classifier_max_per_session: 3,
        }
    }
}

/// Pure gating inputs for the classifier-branch decision. Extracted so the
/// branch logic can be unit-tested without constructing a `TrackedSession`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClassifierTickInputs<'a> {
    pub idle_duration: u64,
    pub provider: SessionProvider,
    pub last_classified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub classification_count: u32,
    pub config: &'a StallConfig,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// True iff the classifier should be signaled for the given session this
/// tick. Encodes the per-provider threshold + cooldown + cap rules from
/// plan §D4 / §Phase 2.
pub(crate) fn should_signal_classifier(i: &ClassifierTickInputs<'_>) -> bool {
    let threshold = match i.provider {
        SessionProvider::Codex
        | SessionProvider::Pioneer
        | SessionProvider::OpenRouter
        | SessionProvider::Bedrock => i.config.classifier_idle_secs_codex,
        _ => i.config.classifier_idle_secs,
    };
    if i.idle_duration < threshold {
        return false;
    }
    if i.classification_count >= i.config.classifier_max_per_session {
        return false;
    }
    if let Some(t) = i.last_classified_at {
        let since = i.now.signed_duration_since(t).num_seconds().max(0) as u64;
        if since < i.config.classifier_cooldown_secs {
            return false;
        }
    }
    true
}

fn should_enqueue_stall_retry(
    runtime_config: &crate::config::RuntimeConfig,
    session_kind: SessionKind,
    max_retries: Option<u8>,
) -> bool {
    retry_policy::session_retries_allowed(runtime_config, session_kind, max_retries)
}

/// Spawn the stall detector background task.
///
/// Scans all active sessions every 60 seconds. Publishes `SessionStalled`
/// events for sessions that have exceeded their idle threshold. Tracks which
/// sessions have already been reported to avoid duplicate events -- only the
/// first crossing of the threshold triggers a bus event.
///
/// When `StallAction::Interrupt` or `StallAction::InterruptAndRetry` is configured,
/// the detector will take remediation action for TaskRabbit/Bug sessions, and
/// optionally retry them via `retry_tx`.
///
/// When `stall_classifier_tx` is `Some(tx)`, sessions idle past the
/// per-provider classifier threshold are signaled on `tx` and the tick
/// short-circuits without taking a static action. See module docs for the
/// ordering invariant.
pub fn spawn_stall_detector(
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    event_bus: Arc<EventBus>,
    config: StallConfig,
    retry_tx: mpsc::Sender<Uuid>,
    stall_classifier_tx: Option<mpsc::Sender<Uuid>>,
    runtime_config: Arc<crate::config::RuntimeConfig>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        // Set to missed-tick behavior that skips missed ticks (avoids burst after sleep)
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Track sessions already reported as stalled to avoid duplicate events
        let mut reported: HashSet<Uuid> = HashSet::new();

        loop {
            interval.tick().await;

            let now = chrono::Utc::now();

            // Snapshot stalled sessions under a read lock (collect IDs + data only)
            let mut newly_stalled: Vec<(Uuid, SessionStatus, u64, SessionKind, StallAction)> =
                Vec::new();
            let mut classifier_signals: Vec<Uuid> = Vec::new();
            let mut active_ids: HashSet<Uuid> = HashSet::new();

            {
                let active_guard = active.read().await;
                for (session_id, tracked) in active_guard.iter() {
                    active_ids.insert(*session_id);

                    let threshold_secs = match tracked.session.status {
                        SessionStatus::Running | SessionStatus::Starting => config.running_secs,
                        SessionStatus::WaitingApproval => config.waiting_secs,
                        _ => continue,
                    };

                    let idle_duration = now
                        .signed_duration_since(tracked.last_event_at)
                        .num_seconds()
                        .max(0) as u64;

                    // Classifier branch (Phase 2). Runs BEFORE the static
                    // gate and rotation guard so it always fires first when
                    // enabled. Does NOT mark `reported` — static path
                    // remains the fallback safety net for telemetry-only
                    // verdicts.
                    if stall_classifier_tx.is_some() {
                        let inputs = ClassifierTickInputs {
                            idle_duration,
                            provider: tracked.session.provider,
                            last_classified_at: tracked.last_classified_at,
                            classification_count: tracked.classification_count,
                            config: &config,
                            now,
                        };
                        if should_signal_classifier(&inputs) {
                            classifier_signals.push(*session_id);
                            continue;
                        }
                    }

                    if idle_duration < threshold_secs || reported.contains(session_id) {
                        continue;
                    }

                    // Skip sessions in active rotation phases
                    let is_rotating = matches!(
                        tracked.rotation.state(),
                        RotationState::PendingInterrupt { .. }
                            | RotationState::WritingHandoff { .. }
                    );
                    if is_rotating {
                        tracing::debug!(
                            session_id = %session_id,
                            "Stall detector: skipping session in rotation"
                        );
                        continue;
                    }

                    // Determine stall action based on session kind
                    let action = if matches!(
                        tracked.session.session_kind,
                        SessionKind::TaskRabbit | SessionKind::Bug
                    ) {
                        config.unattended_action
                    } else {
                        config.standard_action
                    };

                    newly_stalled.push((
                        *session_id,
                        tracked.session.status,
                        idle_duration,
                        tracked.session.session_kind,
                        action,
                    ));
                }
            }

            // Dispatch classifier signals outside the read lock. `try_send`
            // drops the signal if the channel is full or the classifier
            // task has shut down — both are acceptable because the static
            // path remains as the fallback safety net.
            if let Some(ref tx) = stall_classifier_tx {
                for sid in &classifier_signals {
                    if let Err(e) = tx.try_send(*sid) {
                        tracing::debug!(
                            session_id = %sid,
                            error = ?e,
                            "Stall classifier signal dropped (channel full or closed)"
                        );
                    }
                }
            }

            // Process stalled sessions outside the read lock
            for (session_id, status, idle_secs, _session_kind, action) in newly_stalled {
                tracing::info!(
                    session_id = %session_id,
                    status = ?status,
                    idle_secs = idle_secs,
                    action = ?action,
                    "Session stall detected"
                );

                match action {
                    StallAction::Notify => {
                        // Original behavior: publish stalled event only
                        event_bus.publish(DaemonEvent::SessionStalled {
                            session_id,
                            status,
                            idle_secs,
                        });
                    }
                    StallAction::Interrupt => {
                        // Interrupt the process, let monitor loop finalize
                        let mut active_guard = active.write().await;
                        if let Some(tracked) = active_guard.get_mut(&session_id) {
                            tracked.stall_interrupted = true;
                            let old_status = tracked.session.status;
                            if let Some(ref process) = tracked.process {
                                let _ = process.interrupt();
                            }
                            let _ = tracked.stop_tx.try_send(());
                            event_bus.publish(DaemonEvent::SessionReconciled {
                                session_id,
                                old_status,
                                new_status: SessionStatus::Failed,
                                reason: ReconciliationReason::StallRemediation,
                            });
                        }
                    }
                    StallAction::InterruptAndRetry => {
                        // Interrupt + queue retry via retry_tx
                        let mut active_guard = active.write().await;
                        if let Some(tracked) = active_guard.get_mut(&session_id) {
                            tracked.stall_interrupted = true;
                            let old_status = tracked.session.status;
                            if let Some(ref process) = tracked.process {
                                let _ = process.interrupt();
                            }
                            let _ = tracked.stop_tx.try_send(());
                            if should_enqueue_stall_retry(
                                &runtime_config,
                                tracked.session.session_kind,
                                tracked.session.max_retries,
                            ) {
                                // A9.1: the retry channel is bounded (cap 16).
                                // `try_send` fails on Full (queue saturated —
                                // this stall retry is dropped) or Closed (daemon
                                // shutting down). Either way, surface it so a
                                // dropped stall-triggered retry is never silent.
                                if let Err(e) = retry_tx.try_send(session_id) {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        error = %e,
                                        "Stall retry dropped: retry channel unavailable (full or closed)"
                                    );
                                }
                            }
                            event_bus.publish(DaemonEvent::SessionReconciled {
                                session_id,
                                old_status,
                                new_status: SessionStatus::Failed,
                                reason: ReconciliationReason::StallRemediation,
                            });
                        }
                    }
                }

                reported.insert(session_id);
            }

            // Prune reported set: remove sessions no longer active
            reported.retain(|id| active_ids.contains(id));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimeConfig};
    use chrono::Duration as ChronoDuration;
    use rsi_common::types::SessionStatus;
    use std::sync::atomic::Ordering;

    fn cfg() -> StallConfig {
        StallConfig {
            running_secs: 1800,
            waiting_secs: 3600,
            standard_action: StallAction::Notify,
            unattended_action: StallAction::Notify,
            classifier_idle_secs: 600,
            classifier_idle_secs_codex: 1800,
            classifier_cooldown_secs: 1800,
            classifier_max_per_session: 3,
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    #[test]
    fn test_session_stalled_event_type() {
        let event = DaemonEvent::SessionStalled {
            session_id: Uuid::new_v4(),
            status: SessionStatus::Running,
            idle_secs: 1800,
        };
        let bus_event: rsi_common::rpc::BusEvent = event.into();
        assert_eq!(bus_event.event_type, "session_stalled");
    }

    #[test]
    fn test_session_stalled_serde_roundtrip() {
        let event = DaemonEvent::SessionStalled {
            session_id: Uuid::new_v4(),
            status: SessionStatus::Running,
            idle_secs: 1800,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deser: DaemonEvent = serde_json::from_str(&json).unwrap();
        match deser {
            DaemonEvent::SessionStalled { idle_secs, .. } => {
                assert_eq!(idle_secs, 1800);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_stall_config_default() {
        let config = StallConfig::default();
        assert_eq!(config.running_secs, 1800);
        assert_eq!(config.waiting_secs, 3600);
        assert_eq!(config.standard_action, StallAction::Notify);
        assert_eq!(config.unattended_action, StallAction::Notify);
        assert_eq!(config.classifier_idle_secs, 600);
        assert_eq!(config.classifier_idle_secs_codex, 1800);
        assert_eq!(config.classifier_cooldown_secs, 1800);
        assert_eq!(config.classifier_max_per_session, 3);
    }

    #[test]
    fn test_stall_action_imported_from_reconciliation() {
        // Verify StallAction types are correctly imported and usable
        assert_eq!(StallAction::from_str("notify"), StallAction::Notify);
        assert_eq!(StallAction::from_str("interrupt"), StallAction::Interrupt);
        assert_eq!(
            StallAction::from_str("interrupt_and_retry"),
            StallAction::InterruptAndRetry
        );
    }

    #[test]
    fn stall_retry_gate_uses_retry_policy() {
        let runtime = RuntimeConfig::from_config(&Config::default());
        assert!(!should_enqueue_stall_retry(
            &runtime,
            SessionKind::Task,
            Some(1)
        ));
        runtime.retry_enabled.store(true, Ordering::Relaxed);
        assert!(should_enqueue_stall_retry(
            &runtime,
            SessionKind::Task,
            Some(1)
        ));
        assert!(!should_enqueue_stall_retry(
            &runtime,
            SessionKind::Story,
            None
        ));
        assert!(!should_enqueue_stall_retry(
            &runtime,
            SessionKind::Task,
            Some(0)
        ));

        runtime.retry_enabled.store(false, Ordering::Relaxed);
        assert!(!should_enqueue_stall_retry(
            &runtime,
            SessionKind::Task,
            Some(1)
        ));
    }

    // --- Classifier-branch gating (RSI-0XX) ---

    #[test]
    fn classifier_branch_signals_when_threshold_met() {
        let c = cfg();
        let i = ClassifierTickInputs {
            idle_duration: 700,
            provider: SessionProvider::Claude,
            last_classified_at: None,
            classification_count: 0,
            config: &c,
            now: now(),
        };
        assert!(should_signal_classifier(&i));
    }

    #[test]
    fn classifier_branch_silent_below_threshold() {
        let c = cfg();
        let i = ClassifierTickInputs {
            idle_duration: 300,
            provider: SessionProvider::Claude,
            last_classified_at: None,
            classification_count: 0,
            config: &c,
            now: now(),
        };
        assert!(!should_signal_classifier(&i));
    }

    #[test]
    fn classifier_branch_uses_codex_threshold_for_codex_sessions() {
        let c = cfg();
        // 700s clears Claude (600) but not Codex (1800).
        let claude = ClassifierTickInputs {
            idle_duration: 700,
            provider: SessionProvider::Claude,
            last_classified_at: None,
            classification_count: 0,
            config: &c,
            now: now(),
        };
        let codex = ClassifierTickInputs {
            provider: SessionProvider::Codex,
            ..claude
        };
        assert!(should_signal_classifier(&claude));
        assert!(!should_signal_classifier(&codex));
    }

    #[test]
    fn classifier_branch_blocked_by_cap() {
        let c = cfg();
        let i = ClassifierTickInputs {
            idle_duration: 9999,
            provider: SessionProvider::Claude,
            last_classified_at: None,
            classification_count: c.classifier_max_per_session,
            config: &c,
            now: now(),
        };
        assert!(!should_signal_classifier(&i));
    }

    #[test]
    fn classifier_branch_blocked_by_cooldown() {
        let c = cfg();
        let n = now();
        let recent = n - ChronoDuration::seconds(60);
        let inputs = ClassifierTickInputs {
            idle_duration: 9999,
            provider: SessionProvider::Claude,
            last_classified_at: Some(recent),
            classification_count: 0,
            config: &c,
            now: n,
        };
        assert!(!should_signal_classifier(&inputs));
    }

    #[test]
    fn classifier_branch_allows_after_cooldown_expires() {
        let c = cfg();
        let n = now();
        let old = n - ChronoDuration::seconds(c.classifier_cooldown_secs as i64 + 1);
        let inputs = ClassifierTickInputs {
            idle_duration: 9999,
            provider: SessionProvider::Claude,
            last_classified_at: Some(old),
            classification_count: 0,
            config: &c,
            now: n,
        };
        assert!(should_signal_classifier(&inputs));
    }

    #[test]
    fn classifier_branch_blocked_when_provider_codex_below_codex_threshold() {
        let c = cfg();
        let inputs = ClassifierTickInputs {
            idle_duration: 1700,
            provider: SessionProvider::Codex,
            last_classified_at: None,
            classification_count: 0,
            config: &c,
            now: now(),
        };
        assert!(!should_signal_classifier(&inputs));
    }

    #[test]
    fn classifier_branch_uses_codex_threshold_for_pioneer() {
        let c = cfg();
        let inputs = ClassifierTickInputs {
            idle_duration: 1700,
            provider: SessionProvider::Pioneer,
            last_classified_at: None,
            classification_count: 0,
            config: &c,
            now: now(),
        };
        assert!(!should_signal_classifier(&inputs));
    }
}
