//! When the manager coordinator runs a pass (#2494).
//!
//! A pass reads every managed project's cohort and spend, so it is the
//! daemon's costliest periodic read. The 10 s interval tick always runs one.
//! An event only brings the next pass forward: a question or an operator
//! decision at once (an operator waits on it), any other hint (a status,
//! metadata, creation or rate-limit change, all frequent while workers run)
//! only once [`HINT_SPACING`] has passed since the previous pass. A hint
//! dropped here costs at most the rest of the tick interval.

use std::time::Duration;

use crate::bus::DaemonEvent;

/// The shortest gap between two passes when the second is only a hint.
pub const HINT_SPACING: Duration = Duration::from_secs(5);

/// The shortest gap between any two passes.
pub const MIN_SPACING: Duration = Duration::from_secs(1);

/// What an event asks of the coordinator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerWake {
    /// Nothing the coordinator reads changed.
    Ignore,
    /// A change worth a pass once [`HINT_SPACING`] has passed.
    Hint,
    /// An operator-facing change: a pass as soon as [`MIN_SPACING`] allows.
    Urgent,
}

impl ManagerWake {
    #[must_use]
    pub fn classify(event: &DaemonEvent) -> Self {
        match event {
            DaemonEvent::SessionQuestionRaised { .. } => Self::Urgent,
            DaemonEvent::SystemMessage { message, .. }
                if message.starts_with("Harness manager decision") =>
            {
                Self::Urgent
            }
            DaemonEvent::SessionStatusChanged { .. }
            | DaemonEvent::SessionCreated { .. }
            | DaemonEvent::SessionMetadataChanged { .. }
            | DaemonEvent::ProviderRateLimitUpdated { .. } => Self::Hint,
            _ => Self::Ignore,
        }
    }

    /// Whether a wake of this kind runs a pass `since_last_pass` after the
    /// previous one. `tick` is the interval backstop, which always runs.
    #[must_use]
    pub fn runs(self, tick: bool, since_last_pass: Duration) -> bool {
        if since_last_pass < MIN_SPACING {
            return false;
        }
        tick || match self {
            Self::Ignore => false,
            Self::Hint => since_last_pass >= HINT_SPACING,
            Self::Urgent => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn hints_wait_for_the_spacing_but_the_tick_and_urgent_events_do_not_2494() {
        let soon = Duration::from_secs(2);
        let late = HINT_SPACING;
        assert!(!ManagerWake::Hint.runs(false, soon));
        assert!(ManagerWake::Hint.runs(false, late));
        assert!(ManagerWake::Urgent.runs(false, soon));
        assert!(ManagerWake::Ignore.runs(true, soon));
        assert!(!ManagerWake::Ignore.runs(false, late));
        // No wake runs two passes inside the minimum spacing.
        assert!(!ManagerWake::Urgent.runs(false, Duration::from_millis(500)));
        assert!(!ManagerWake::Ignore.runs(true, Duration::from_millis(500)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn events_classify_by_who_waits_on_them_2494() {
        let decision = DaemonEvent::SystemMessage {
            level: "info".into(),
            message: "Harness manager decision d1: operator answer recorded".into(),
        };
        assert_eq!(ManagerWake::classify(&decision), ManagerWake::Urgent);
        let other = DaemonEvent::SystemMessage {
            level: "info".into(),
            message: "something else".into(),
        };
        assert_eq!(ManagerWake::classify(&other), ManagerWake::Ignore);
    }
}
