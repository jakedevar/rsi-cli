//! Single-owner state machine for context rotation.
//!
//! Replaces the scattered `RotationPhase` enum + `rotation_limit_hit` bool +
//! `handoff_filepath_detected` Option that were previously spread across
//! `TrackedSession` fields. All rotation decision logic is now centralized here.

use super::types::MonitorBreakReason;
use uuid::Uuid;

/// Per-phase timeout constants.
pub(super) const PENDING_INTERRUPT_TIMEOUT_SECS: u64 = 30;
pub(super) const WRITING_HANDOFF_TIMEOUT_SECS: u64 = 300;

/// The phase of context rotation a session is currently in.
#[derive(Debug, Clone)]
pub(crate) enum RotationState {
    /// Not in any rotation flow. Initial state.
    Idle,
    /// Context hit 65%, stop signal sent. Waiting for monitor loop to break.
    PendingInterrupt { deadline: tokio::time::Instant },
    /// Session resumed with /create_handoff. Waiting for handoff doc to be written.
    WritingHandoff {
        handoff_filepath: Option<String>,
        deadline: tokio::time::Instant,
    },
    /// Terminal state: rotation completed.
    Completed,
}

/// Events the coordinator receives from the monitor loop.
#[derive(Debug)]
pub(super) enum RotationEvent {
    /// Token usage updated.
    ThresholdCheck { pct: f64 },
    /// User explicitly requested manual rotation.
    /// This bypasses auto-rotation enablement.
    ManualTrigger,
    /// A handoff filepath was detected in Write tool_use or assistant text.
    HandoffFileDetected { path: String },
    /// The current rotation phase deadline elapsed.
    DeadlineElapsed,
    /// The monitor loop exited.
    MonitorCompleted { break_reason: MonitorBreakReason },
}

/// Actions the monitor loop must execute in response to a coordinator decision.
#[derive(Debug)]
pub(super) enum RotationAction {
    /// No rotation action needed.
    NoOp,
    /// Send stop signal to interrupt the running session for handoff writing.
    InterruptForRotation,
    /// Finalize the session then re-launch it with /create_handoff.
    SendCreateHandoff,
    /// Finalize the session then spawn the rotation child.
    SpawnChild {
        session_id: Uuid,
        handoff_filepath: Option<String>,
    },
    /// Break the monitor loop because the current rotation phase timed out.
    BreakMonitor {
        phase: &'static str,
        break_reason: MonitorBreakReason,
        kill_process: bool,
    },
}

/// Single-owner state machine for context rotation.
/// One coordinator exists per session (stored on `TrackedSession`).
#[derive(Debug)]
pub(crate) struct RotationCoordinator {
    pub(super) session_id: Uuid,
    pub(super) state: RotationState,
    enabled: bool,
    /// Unique ID grouping all events for one rotation attempt.
    /// Generated lazily when the first non-idle transition occurs.
    rotation_id: Option<String>,
}

impl RotationCoordinator {
    fn pending_interrupt_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(PENDING_INTERRUPT_TIMEOUT_SECS)
    }

    fn writing_handoff_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(WRITING_HANDOFF_TIMEOUT_SECS)
    }

    fn pending_interrupt_state() -> RotationState {
        RotationState::PendingInterrupt {
            deadline: Self::pending_interrupt_deadline(),
        }
    }

    fn writing_handoff_state(handoff_filepath: Option<String>) -> RotationState {
        RotationState::WritingHandoff {
            handoff_filepath,
            deadline: Self::writing_handoff_deadline(),
        }
    }

    /// Create a coordinator for a fresh or continued session in `Idle` state.
    pub(crate) const fn new(session_id: Uuid, _depth: u32, enabled: bool) -> Self {
        Self {
            session_id,
            state: RotationState::Idle,
            enabled,
            rotation_id: None,
        }
    }

    /// Create a coordinator already in `WritingHandoff` state.
    pub(crate) fn new_writing_handoff(session_id: Uuid, depth: u32, enabled: bool) -> Self {
        Self::new_writing_handoff_with_rotation_id(session_id, depth, enabled, None)
    }

    /// Create a coordinator already in `WritingHandoff` state with an explicit rotation ID.
    pub(crate) fn new_writing_handoff_with_rotation_id(
        session_id: Uuid,
        _depth: u32,
        enabled: bool,
        rotation_id: Option<String>,
    ) -> Self {
        Self {
            session_id,
            state: Self::writing_handoff_state(None),
            enabled,
            rotation_id: Some(rotation_id.unwrap_or_else(|| Uuid::new_v4().to_string())),
        }
    }

    /// Get the current rotation state.
    pub(crate) fn state(&self) -> &RotationState {
        &self.state
    }

    /// Returns true if this coordinator is in a non-idle rotation state.
    pub(crate) fn is_rotating(&self) -> bool {
        !matches!(self.state, RotationState::Idle)
    }

    /// Update the enabled flag at runtime (e.g., when per-session toggle changes).
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns the rotation_id for this rotation attempt, if one is active.
    pub(crate) fn rotation_id(&self) -> Option<&str> {
        self.rotation_id.as_deref()
    }

    /// Returns the current phase deadline, if this state is deadline-driven.
    pub(crate) fn current_deadline(&self) -> Option<tokio::time::Instant> {
        match &self.state {
            RotationState::PendingInterrupt { deadline }
            | RotationState::WritingHandoff { deadline, .. } => Some(*deadline),
            RotationState::Idle | RotationState::Completed => None,
        }
    }

    /// Ensure rotation_id is set, creating one if needed.
    fn ensure_rotation_id(&mut self) -> &str {
        if self.rotation_id.is_none() {
            self.rotation_id = Some(Uuid::new_v4().to_string());
        }
        self.rotation_id.as_deref().unwrap()
    }

    /// Advance the state machine given an event.
    /// Returns the action the caller must execute.
    pub(super) fn advance(&mut self, event: RotationEvent) -> RotationAction {
        match (&self.state, event) {
            // ── Threshold check ──
            (RotationState::Idle, RotationEvent::ThresholdCheck { pct }) if self.enabled => {
                if pct >= 65.0 {
                    self.ensure_rotation_id();
                    self.state = Self::pending_interrupt_state();
                    RotationAction::InterruptForRotation
                } else {
                    RotationAction::NoOp
                }
            }

            // ── Manual rotation request ──
            (RotationState::Idle, RotationEvent::ManualTrigger) => {
                self.ensure_rotation_id();
                self.state = Self::pending_interrupt_state();
                RotationAction::InterruptForRotation
            }

            // ── Handoff file detected during WritingHandoff ──
            (
                RotationState::WritingHandoff { deadline, .. },
                RotationEvent::HandoffFileDetected { path },
            ) => {
                self.state = RotationState::WritingHandoff {
                    handoff_filepath: Some(path),
                    deadline: *deadline,
                };
                RotationAction::NoOp
            }

            // ── Deadline expired while waiting for interrupt ──
            (RotationState::PendingInterrupt { .. }, RotationEvent::DeadlineElapsed) => {
                RotationAction::BreakMonitor {
                    phase: "pending_interrupt",
                    break_reason: MonitorBreakReason::Rotation,
                    kill_process: true,
                }
            }

            // ── Deadline expired while waiting for handoff write ──
            (RotationState::WritingHandoff { .. }, RotationEvent::DeadlineElapsed) => {
                RotationAction::BreakMonitor {
                    phase: "writing_handoff",
                    break_reason: MonitorBreakReason::StreamClosed,
                    kill_process: false,
                }
            }

            // ── Monitor completed: PendingInterrupt → send /create_handoff ──
            (
                RotationState::PendingInterrupt { .. },
                RotationEvent::MonitorCompleted { break_reason },
            ) if matches!(
                break_reason,
                MonitorBreakReason::Rotation
                    | MonitorBreakReason::Result
                    | MonitorBreakReason::StreamClosed
            ) =>
            {
                self.state = Self::writing_handoff_state(None);
                RotationAction::SendCreateHandoff
            }

            // ── Monitor completed: WritingHandoff → spawn child ──
            (
                RotationState::WritingHandoff {
                    handoff_filepath, ..
                },
                RotationEvent::MonitorCompleted { break_reason },
            ) if matches!(
                break_reason,
                MonitorBreakReason::Result | MonitorBreakReason::StreamClosed
            ) =>
            {
                let path = handoff_filepath.clone();
                self.state = RotationState::Completed;
                RotationAction::SpawnChild {
                    session_id: self.session_id,
                    handoff_filepath: path,
                }
            }

            // ── Monitor completed: user interrupted during rotation ──
            (
                _,
                RotationEvent::MonitorCompleted {
                    break_reason: MonitorBreakReason::Interrupted,
                },
            ) => RotationAction::NoOp,

            // ── All other combinations ──
            _ => RotationAction::NoOp,
        }
    }
}

/// Returns true if `path` is a handoff document path.
pub(crate) fn is_handoff_file(path: &str) -> bool {
    let p = std::path::Path::new(path);
    p.starts_with("thoughts/shared/handoffs") && p.extension() == Some(std::ffi::OsStr::new("md"))
}

/// Returns true if `path` is a pipeline artifact (research or plan doc).
pub(crate) fn is_pipeline_artifact(path: &str) -> bool {
    let p = std::path::Path::new(path);
    (p.starts_with("thoughts/shared/research") || p.starts_with("thoughts/shared/plans"))
        && p.extension() == Some(std::ffi::OsStr::new("md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_below_threshold_is_noop() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, true);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 50.0 }),
            RotationAction::NoOp
        ));
        assert!(matches!(c.state, RotationState::Idle));
    }

    #[test]
    fn idle_above_65_depth0_interrupts() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, true);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 70.0 }),
            RotationAction::InterruptForRotation
        ));
        assert!(matches!(c.state, RotationState::PendingInterrupt { .. }));
        assert!(c.current_deadline().is_some());
    }

    #[test]
    fn idle_above_65_depth_four_rotates() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 4, true);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 70.0 }),
            RotationAction::InterruptForRotation
        ));
        assert!(matches!(c.state, RotationState::PendingInterrupt { .. }));
    }

    #[test]
    fn pending_interrupt_rotation_sends_create_handoff() {
        let sid = Uuid::new_v4();
        let mut c = RotationCoordinator::new(sid, 0, true);
        c.state = RotationState::PendingInterrupt {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        };
        assert!(matches!(
            c.advance(RotationEvent::MonitorCompleted {
                break_reason: MonitorBreakReason::Rotation
            }),
            RotationAction::SendCreateHandoff
        ));
        assert!(matches!(c.state, RotationState::WritingHandoff { .. }));
    }

    #[test]
    fn pending_interrupt_timeout_breaks_monitor_and_kills_process() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, true);
        c.state = RotationState::PendingInterrupt {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        };
        assert!(matches!(
            c.advance(RotationEvent::DeadlineElapsed),
            RotationAction::BreakMonitor {
                phase: "pending_interrupt",
                break_reason: MonitorBreakReason::Rotation,
                kill_process: true,
            }
        ));
    }

    #[test]
    fn writing_handoff_timeout_breaks_monitor_without_killing_process() {
        let mut c = RotationCoordinator::new_writing_handoff(Uuid::new_v4(), 0, true);
        assert!(matches!(
            c.advance(RotationEvent::DeadlineElapsed),
            RotationAction::BreakMonitor {
                phase: "writing_handoff",
                break_reason: MonitorBreakReason::StreamClosed,
                kill_process: false,
            }
        ));
    }

    #[test]
    fn writing_handoff_detects_file() {
        let mut c = RotationCoordinator::new_writing_handoff(Uuid::new_v4(), 0, true);
        c.advance(RotationEvent::HandoffFileDetected {
            path: "thoughts/shared/handoffs/test.md".to_string(),
        });
        assert!(matches!(
            c.state,
            RotationState::WritingHandoff {
                handoff_filepath: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn writing_handoff_with_path_spawns_child() {
        let mut c = RotationCoordinator::new_writing_handoff(Uuid::new_v4(), 0, true);
        c.state = RotationState::WritingHandoff {
            handoff_filepath: Some("test.md".to_string()),
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(300),
        };
        assert!(matches!(
            c.advance(RotationEvent::MonitorCompleted {
                break_reason: MonitorBreakReason::Result
            }),
            RotationAction::SpawnChild {
                handoff_filepath: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn writing_handoff_without_path_spawns_with_none() {
        let mut c = RotationCoordinator::new_writing_handoff(Uuid::new_v4(), 0, true);
        assert!(matches!(
            c.advance(RotationEvent::MonitorCompleted {
                break_reason: MonitorBreakReason::Result
            }),
            RotationAction::SpawnChild {
                handoff_filepath: None,
                ..
            }
        ));
    }

    #[test]
    fn disabled_rotation_is_noop() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, false);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 90.0 }),
            RotationAction::NoOp
        ));
    }

    #[test]
    fn set_enabled_toggles_threshold_behavior() {
        // Initially enabled — should trigger
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, true);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 70.0 }),
            RotationAction::InterruptForRotation
        ));

        // Fresh coordinator, then disable — should be NoOp
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, true);
        c.set_enabled(false);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 90.0 }),
            RotationAction::NoOp
        ));

        // Re-enable — should trigger again
        c.set_enabled(true);
        assert!(matches!(
            c.advance(RotationEvent::ThresholdCheck { pct: 70.0 }),
            RotationAction::InterruptForRotation
        ));
    }

    #[test]
    fn rotation_id_generated_on_threshold() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, true);
        assert!(c.rotation_id().is_none());
        c.advance(RotationEvent::ThresholdCheck { pct: 70.0 });
        assert!(c.rotation_id().is_some());
    }

    #[test]
    fn manual_trigger_interrupts_even_when_auto_rotation_disabled() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 0, false);
        assert!(matches!(
            c.advance(RotationEvent::ManualTrigger),
            RotationAction::InterruptForRotation
        ));
        assert!(matches!(c.state, RotationState::PendingInterrupt { .. }));
        assert!(c.rotation_id().is_some());
        assert!(c.current_deadline().is_some());
    }

    #[test]
    fn manual_trigger_is_independent_of_rotation_depth() {
        let mut c = RotationCoordinator::new(Uuid::new_v4(), 4, true);
        assert!(matches!(
            c.advance(RotationEvent::ManualTrigger),
            RotationAction::InterruptForRotation
        ));
        assert!(matches!(c.state, RotationState::PendingInterrupt { .. }));
    }

    #[test]
    fn rotation_id_preset_for_writing_handoff() {
        let c = RotationCoordinator::new_writing_handoff(Uuid::new_v4(), 0, true);
        assert!(c.rotation_id().is_some());
    }

    #[test]
    fn writing_handoff_preserves_supplied_rotation_id() {
        let c = RotationCoordinator::new_writing_handoff_with_rotation_id(
            Uuid::new_v4(),
            0,
            true,
            Some("rotation-123".to_string()),
        );
        assert_eq!(c.rotation_id(), Some("rotation-123"));
    }

    #[test]
    fn is_handoff_file_matches() {
        assert!(is_handoff_file(
            "thoughts/shared/handoffs/general/2026-03-13_test.md"
        ));
        assert!(is_handoff_file(
            "thoughts/shared/handoffs/ENG-1234/2026-03-13_test.md"
        ));
        assert!(!is_handoff_file(
            "thoughts/shared/research/2026-03-13-test.md"
        ));
        assert!(!is_handoff_file("thoughts/shared/handoffs/test.txt"));
        assert!(!is_handoff_file("src/main.rs"));
    }

    #[test]
    fn is_pipeline_artifact_matches() {
        assert!(is_pipeline_artifact(
            "thoughts/shared/research/2026-03-13-test.md"
        ));
        assert!(is_pipeline_artifact(
            "thoughts/shared/plans/2026-03-13-test.md"
        ));
        assert!(!is_pipeline_artifact("thoughts/shared/handoffs/test.md"));
        assert!(!is_pipeline_artifact("src/main.rs"));
    }
}
