//! Multi-turn continuation controller for app-server sessions.
//!
//! `TurnController` manages multi-turn execution on app-server sessions.
//! After `turn/completed`, the controller decides whether to issue another
//! `turn/start` based on a `ContinuationPolicy`.

/// Policy governing multi-turn continuation on app-server sessions.
#[derive(Debug, Clone)]
pub enum ContinuationPolicy {
    /// Run exactly one turn then complete (default for Standard sessions).
    Single,
    /// Continue up to N turns on the same thread.
    MaxTurns(u32),
}

/// Controls multi-turn continuation decisions for an app-server session.
pub struct TurnController {
    policy: ContinuationPolicy,
    current_turn: u32,
    max_turns: Option<u32>,
}

impl TurnController {
    pub fn new(policy: ContinuationPolicy) -> Self {
        let max_turns = match &policy {
            ContinuationPolicy::Single => Some(1),
            ContinuationPolicy::MaxTurns(n) => Some(*n),
        };
        Self {
            policy,
            current_turn: 0,
            max_turns,
        }
    }

    /// Record that a turn completed.
    ///
    /// Returns `true` if another turn should be started (continuation allowed),
    /// `false` if the session should complete.
    pub fn turn_completed(&mut self) -> bool {
        self.current_turn += 1;
        match self.max_turns {
            Some(max) => self.current_turn < max,
            None => false,
        }
    }

    /// Current completed turn count (0 before any turn completes).
    pub fn current_turn(&self) -> u32 {
        self.current_turn
    }

    /// The configured continuation policy.
    pub fn policy(&self) -> &ContinuationPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_policy_turn_completed_returns_false() {
        let mut ctrl = TurnController::new(ContinuationPolicy::Single);
        // First (and only) turn completes — no continuation
        assert!(!ctrl.turn_completed());
    }

    #[test]
    fn test_max_turns_3_returns_true_for_turns_1_and_2_false_for_3() {
        let mut ctrl = TurnController::new(ContinuationPolicy::MaxTurns(3));
        // Turn 1 → continue (current=1 < 3)
        assert!(ctrl.turn_completed());
        // Turn 2 → continue (current=2 < 3)
        assert!(ctrl.turn_completed());
        // Turn 3 → stop (current=3 == 3)
        assert!(!ctrl.turn_completed());
    }

    #[test]
    fn test_current_turn_tracks_correctly() {
        let mut ctrl = TurnController::new(ContinuationPolicy::MaxTurns(5));
        assert_eq!(ctrl.current_turn(), 0);
        ctrl.turn_completed();
        assert_eq!(ctrl.current_turn(), 1);
        ctrl.turn_completed();
        assert_eq!(ctrl.current_turn(), 2);
    }

    #[test]
    fn test_max_turns_1_behaves_like_single() {
        let mut ctrl = TurnController::new(ContinuationPolicy::MaxTurns(1));
        // Only 1 allowed, after it completes → stop
        assert!(!ctrl.turn_completed());
    }

    #[test]
    fn test_max_turns_5_allows_five_turns() {
        let mut ctrl = TurnController::new(ContinuationPolicy::MaxTurns(5));
        // Turns 1-4 → continue
        for _ in 0..4 {
            assert!(ctrl.turn_completed());
        }
        // Turn 5 → stop
        assert!(!ctrl.turn_completed());
        assert_eq!(ctrl.current_turn(), 5);
    }
}
