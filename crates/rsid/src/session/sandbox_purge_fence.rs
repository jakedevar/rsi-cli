//! Retained-candidate fence for the periodic archived-sandbox purge (hub #2940).
//!
//! The purge loop walks every archived sandbox once per cycle. A candidate the
//! pass retains (a scheduled job or Session still reaches its root, a pending
//! consumer, an unlanded tip still inside its retention window, a proof that
//! fails) is retained again on the next cycle for the same reason, yet every
//! cycle pays its whole proof again: the worktree status, the tree and holder
//! proofs, the repository proof and the full-corpus dependency scan.
//!
//! This fence remembers, in memory, that a candidate was retained and skips
//! re-proving the *identical* candidate for a backoff window that doubles with
//! each consecutive unchanged retention up to a ceiling. It is valid because it
//! only ever re-uses a verdict that keeps the sandbox:
//!
//! * a skipped candidate is retained, never purged, so a skip cannot delete
//!   anything and removes no recent-activity protection;
//! * any change to the candidate row (generation, root, branch, source commit,
//!   `updated_at`, ...) is a different candidate and is proved in full at once;
//! * a purge always runs the complete proof chain: the fence is consulted only
//!   to decide whether to *start* a proof, never to finish one;
//! * progress is eventual: the window is capped at [`MAX_WINDOW`], so a
//!   candidate whose blocker cleared outside the row (a job disabled, a tree
//!   cleaned) is re-proved at most that long after its last proof;
//! * the operator RPC and every dry run bypass it, and the loop clears it
//!   whenever the purge is switched off, so re-enabling re-proves everything.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::store::sandbox_reclaim::ArchivedSandboxPurgeCandidate;

/// First backoff after a candidate is retained.
pub(super) const BASE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Longest a retained, unchanged candidate goes without a full proof.
pub(super) const MAX_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Most entries kept. A full map is cleared, which only re-proves candidates.
const MAX_ENTRIES: usize = 4096;

/// Retain codes that are cheap to re-evaluate and flip quickly, so fencing them
/// would only delay progress.
const UNFENCED_RETAIN_CODES: &[&str] = &["active_session"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FenceDecision {
    /// Prove the candidate in full.
    Examine,
    /// Unchanged since a retaining proof, inside its backoff window.
    Fenced,
}

#[derive(Debug)]
struct FenceEntry {
    candidate: ArchivedSandboxPurgeCandidate,
    examined_at: Instant,
    /// Consecutive retaining proofs of this unchanged candidate.
    retains: u32,
}

#[derive(Debug)]
pub(super) struct RetainFence {
    entries: HashMap<Uuid, FenceEntry>,
    base: Duration,
    max: Duration,
}

impl Default for RetainFence {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            base: BASE_WINDOW,
            max: MAX_WINDOW,
        }
    }
}

/// Backoff after `retains` consecutive unchanged retentions: `base`, twice
/// that, and so on up to `max` (15, 30, 60, 60... minutes by default).
fn window_after(base: Duration, max: Duration, retains: u32) -> Duration {
    let doublings = retains.saturating_sub(1).min(8);
    base.saturating_mul(1_u32 << doublings).min(max)
}

impl RetainFence {
    /// A fence with its own windows; a zero window never fences.
    #[cfg(all(
        test,
        any(not(feature = "test-shard-mode"), feature = "test-shard-session-02")
    ))]
    pub(super) fn with_windows(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            ..Self::default()
        }
    }

    pub(super) fn decide(
        &self,
        candidate: &ArchivedSandboxPurgeCandidate,
        now: Instant,
    ) -> FenceDecision {
        let Some(entry) = self.entries.get(&candidate.session_id) else {
            return FenceDecision::Examine;
        };
        if entry.candidate != *candidate {
            return FenceDecision::Examine;
        }
        if now.saturating_duration_since(entry.examined_at)
            >= window_after(self.base, self.max, entry.retains)
        {
            return FenceDecision::Examine;
        }
        FenceDecision::Fenced
    }

    /// A proof of `candidate` at `now` retained it for `code` (or failed).
    pub(super) fn record_retained(
        &mut self,
        candidate: &ArchivedSandboxPurgeCandidate,
        code: &str,
        now: Instant,
    ) {
        if UNFENCED_RETAIN_CODES.contains(&code) {
            self.entries.remove(&candidate.session_id);
            return;
        }
        if self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(&candidate.session_id) {
            self.entries.clear();
        }
        let retains = match self.entries.get(&candidate.session_id) {
            Some(entry) if entry.candidate == *candidate => entry.retains.saturating_add(1),
            _ => 1,
        };
        self.entries.insert(
            candidate.session_id,
            FenceEntry {
                candidate: candidate.clone(),
                examined_at: now,
                retains,
            },
        );
    }

    /// The candidate was purged: nothing to remember.
    pub(super) fn forget(&mut self, session_id: Uuid) {
        self.entries.remove(&session_id);
    }

    /// Drop every entry whose candidate is no longer listed, so the map is
    /// bounded by the live candidate set.
    pub(super) fn retain_listed(&mut self, listed: &HashSet<Uuid>) {
        self.entries.retain(|id, _| listed.contains(id));
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(all(
        test,
        any(not(feature = "test-shard-mode"), feature = "test-shard-session-02")
    ))]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    fn candidate(updated_at: &str) -> ArchivedSandboxPurgeCandidate {
        ArchivedSandboxPurgeCandidate {
            custody_id: Uuid::from_u128(1),
            generation: 3,
            allocation_id: Uuid::from_u128(2),
            session_id: Uuid::from_u128(3),
            updated_at: updated_at.to_string(),
            canonical_repo_dir: "/repo".into(),
            sandbox_root: "/sandboxes/a".into(),
            sandbox_branch: "rsi/a".into(),
            source_commit: "0".repeat(40),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn windows_double_from_the_base_up_to_the_ceiling() {
        let minutes = |retains| window_after(BASE_WINDOW, MAX_WINDOW, retains).as_secs() / 60;
        assert_eq!(
            [
                minutes(0),
                minutes(1),
                minutes(2),
                minutes(3),
                minutes(4),
                minutes(40)
            ],
            [15, 15, 30, 60, 60, 60]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn an_unseen_candidate_is_examined_and_a_retained_one_is_fenced_inside_its_window() {
        let start = Instant::now();
        let mut fence = RetainFence::default();
        let c = candidate("2026-01-01T00:00:00Z");
        assert_eq!(fence.decide(&c, start), FenceDecision::Examine);
        fence.record_retained(&c, "external_dependency", start);
        let almost = start + BASE_WINDOW - Duration::from_secs(1);
        assert_eq!(fence.decide(&c, almost), FenceDecision::Fenced);
        let due = start + BASE_WINDOW;
        assert_eq!(fence.decide(&c, due), FenceDecision::Examine);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn consecutive_unchanged_retentions_widen_the_window_to_the_ceiling() {
        let start = Instant::now();
        let mut fence = RetainFence::default();
        let c = candidate("2026-01-01T00:00:00Z");
        let mut now = start;
        for expected in [15, 30, 60, 60] {
            fence.record_retained(&c, "pending_consumer", now);
            let before = now + Duration::from_secs(expected * 60 - 1);
            assert_eq!(fence.decide(&c, before), FenceDecision::Fenced);
            now += Duration::from_secs(expected * 60);
            assert_eq!(fence.decide(&c, now), FenceDecision::Examine);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_changed_candidate_row_is_examined_at_once_and_restarts_the_backoff() {
        let start = Instant::now();
        let mut fence = RetainFence::default();
        let c = candidate("2026-01-01T00:00:00Z");
        fence.record_retained(&c, "external_dependency", start);
        fence.record_retained(&c, "external_dependency", start);
        let changed = candidate("2026-01-02T00:00:00Z");
        assert_eq!(fence.decide(&changed, start), FenceDecision::Examine);
        fence.record_retained(&changed, "external_dependency", start);
        let first_window = start + BASE_WINDOW - Duration::from_secs(1);
        assert_eq!(fence.decide(&changed, first_window), FenceDecision::Fenced);
        assert_eq!(
            fence.decide(&changed, start + BASE_WINDOW),
            FenceDecision::Examine
        );
        let mut regenerated = changed.clone();
        regenerated.generation += 1;
        assert_eq!(fence.decide(&regenerated, start), FenceDecision::Examine);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn cheap_retain_codes_and_purged_candidates_are_never_remembered() {
        let start = Instant::now();
        let mut fence = RetainFence::default();
        let c = candidate("2026-01-01T00:00:00Z");
        fence.record_retained(&c, "external_dependency", start);
        assert_eq!(fence.len(), 1);
        fence.record_retained(&c, "active_session", start);
        assert_eq!(fence.len(), 0);
        assert_eq!(fence.decide(&c, start), FenceDecision::Examine);
        fence.record_retained(&c, "worktree_dirty", start);
        fence.forget(c.session_id);
        assert_eq!(fence.len(), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_zero_window_never_fences() {
        let start = Instant::now();
        let mut fence = RetainFence::with_windows(Duration::ZERO, Duration::ZERO);
        let c = candidate("2026-01-01T00:00:00Z");
        fence.record_retained(&c, "external_dependency", start);
        assert_eq!(fence.decide(&c, start), FenceDecision::Examine);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn unlisted_candidates_are_dropped_and_clear_forgets_everything() {
        let start = Instant::now();
        let mut fence = RetainFence::default();
        let kept = candidate("2026-01-01T00:00:00Z");
        let mut gone = candidate("2026-01-01T00:00:00Z");
        gone.session_id = Uuid::from_u128(9);
        fence.record_retained(&kept, "proof_failed", start);
        fence.record_retained(&gone, "proof_failed", start);
        fence.retain_listed(&HashSet::from([kept.session_id]));
        assert_eq!(fence.len(), 1);
        assert_eq!(fence.decide(&kept, start), FenceDecision::Fenced);
        fence.clear();
        assert_eq!(fence.decide(&kept, start), FenceDecision::Examine);
    }
}
