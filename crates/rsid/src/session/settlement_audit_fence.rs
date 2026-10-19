//! Retaining-verdict fence for the automatic source-worktree settlement audit
//! (hub #3031).
//!
//! The automatic pass audits every cohort that has a terminal root on each 300 s
//! tick while disk pressure lasts. The audit is the expensive part (the holder
//! inventory reads `/proc`, every root is git-checked, the target is fetched),
//! yet a cohort whose roots are all retained (dirty, unlanded, held) is retained
//! again on the next tick for the same reason: 4-12 CPU-s per pass on the
//! laptop, found by the #2940 measurement.
//!
//! This fence remembers, in memory, that an audit of a cohort returned a
//! *non-applyable* verdict and skips re-auditing the cohort while its inputs
//! are identical, for a backoff window that doubles with each consecutive
//! unchanged retention up to a ceiling. It is valid because:
//!
//! * a skip re-uses only a verdict that changes nothing: a non-applyable audit
//!   plans no apply, so skipping it cannot settle, move or delete anything. The
//!   apply path, its holder proofs and the #2630 persistence streak are not
//!   touched (a successful non-applyable audit observes no outcome, so skipping
//!   one observes none either; a failed audit is never remembered and is
//!   repeated, and counted, every pass);
//! * the inputs are the store inventory the audit starts from (every root row,
//!   its Session status, `updated_at`, dependency digests, resume hold), the
//!   latest settlement run, and the runtime holders that can reach a root (an
//!   active Session that owns one, an active working directory or sandbox root
//!   inside one). Any change is a different cohort and is audited at once;
//! * the observed settlement target tip (the resolved SHA of the ref the audit
//!   compares ancestry against, read from the local ref without a fetch or a
//!   git scan) is an input too, so a root that becomes an integrated ancestor
//!   when the target moves is re-audited on the next tick. A tip that cannot
//!   be read gives no inputs and so audits (fail open). The audit's own fetch
//!   is skipped while fenced, so a tip only moves here when something else
//!   updates the ref;
//! * what the fence cannot see (an external process holding a root, a dirty
//!   tree cleaned on disk, a target the daemon has not fetched yet) is bounded
//!   by [`MAX_WINDOW`]: a retained cohort is audited at least that often;
//! * the fence is cleared when pressure resolves, so a new pressure episode
//!   audits every cohort on its first tick.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::store::cohort_settlement::SourceWorktreeInventoryRow;

/// First backoff after an audit retained a cohort (two 300 s ticks).
pub(super) const BASE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Longest a retained, unchanged cohort goes without an audit. Short because
/// the pass only runs under disk pressure and an input the fence cannot see
/// (an external holder, a tree cleaned on disk, a target tip not yet fetched
/// locally) can make a root eligible.
pub(super) const MAX_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Most cohorts kept. A full map is cleared, which only re-audits cohorts.
const MAX_ENTRIES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FenceDecision {
    /// Audit the cohort in full.
    Audit,
    /// Unchanged since a retaining audit, inside its backoff window.
    Fenced,
}

/// A runtime path that can reach a cohort root: `(session, field, raw path)`.
pub(super) type RuntimeHolder = (Uuid, &'static str, PathBuf);

/// Everything the audit's verdict is derived from that the daemon can read
/// without a `/proc` scan or git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuditInputs {
    inventory: Vec<SourceWorktreeInventoryRow>,
    latest_run: Option<Uuid>,
    holders: Vec<RuntimeHolder>,
    /// The settlement target tip the audit compares ancestry against.
    target_tip: String,
}

impl AuditInputs {
    /// `inventory` is the audit's own starting inventory (after the runtime
    /// completed-drift exclusion); `active_ids` and `paths` are the daemon's
    /// live sessions and their working directories and sandbox roots.
    /// `target_tip` is the resolved settlement target tip (never empty: a tip
    /// that cannot be read builds no inputs at all).
    pub(super) fn new(
        mut inventory: Vec<SourceWorktreeInventoryRow>,
        latest_run: Option<Uuid>,
        target_tip: &str,
        active_ids: &HashSet<Uuid>,
        paths: Vec<RuntimeHolder>,
    ) -> Self {
        inventory.sort_by_key(|row| row.custody_id);
        let holders = relevant_holders(&inventory, active_ids, paths);
        Self {
            inventory,
            latest_run,
            holders,
            target_tip: target_tip.to_string(),
        }
    }

    #[cfg(all(
        test,
        any(not(feature = "test-shard-mode"), feature = "test-shard-session-02")
    ))]
    pub(super) fn holders(&self) -> &[RuntimeHolder] {
        &self.holders
    }
}

/// The runtime holders that can reach one of `inventory`'s roots: an active
/// owner of a row, and a path that lies inside a root or that is relative (and
/// so cannot be placed). A lexical test: a path reaching a root through a
/// symlink is not seen here and is covered by the window ceiling.
fn relevant_holders(
    inventory: &[SourceWorktreeInventoryRow],
    active_ids: &HashSet<Uuid>,
    paths: Vec<RuntimeHolder>,
) -> Vec<RuntimeHolder> {
    let owners = inventory
        .iter()
        .flat_map(|row| [row.session_id, row.owner_session_id])
        .flatten()
        .filter(|id| active_ids.contains(id))
        .collect::<HashSet<_>>();
    let roots = inventory
        .iter()
        .map(|row| Path::new(&row.sandbox_root))
        .collect::<Vec<_>>();
    let mut holders = paths
        .into_iter()
        .filter(|(session, _, path)| {
            owners.contains(session)
                || !path.is_absolute()
                || roots.iter().any(|root| path.starts_with(root))
        })
        .collect::<Vec<_>>();
    for owner in owners {
        if !holders.iter().any(|(session, _, _)| *session == owner) {
            holders.push((owner, "active", PathBuf::new()));
        }
    }
    holders.sort();
    holders.dedup();
    holders
}

#[derive(Debug)]
struct FenceEntry {
    inputs: AuditInputs,
    audited_at: Instant,
    /// Consecutive non-applyable audits of these identical inputs.
    retains: u32,
}

#[derive(Debug)]
pub(crate) struct AuditFence {
    entries: HashMap<String, FenceEntry>,
    base: Duration,
    max: Duration,
}

impl Default for AuditFence {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            base: BASE_WINDOW,
            max: MAX_WINDOW,
        }
    }
}

/// Backoff after `retains` consecutive unchanged retentions: `base`, twice
/// that, and so on up to `max` (10, 20, 30, 30... minutes by default).
fn window_after(base: Duration, max: Duration, retains: u32) -> Duration {
    let doublings = retains.saturating_sub(1).min(8);
    base.saturating_mul(1_u32 << doublings).min(max)
}

impl AuditFence {
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
        repository_identity: &str,
        inputs: &AuditInputs,
        now: Instant,
    ) -> FenceDecision {
        let Some(entry) = self.entries.get(repository_identity) else {
            return FenceDecision::Audit;
        };
        if entry.inputs != *inputs {
            return FenceDecision::Audit;
        }
        if now.saturating_duration_since(entry.audited_at)
            >= window_after(self.base, self.max, entry.retains)
        {
            return FenceDecision::Audit;
        }
        FenceDecision::Fenced
    }

    /// An audit of the cohort at `now` found nothing to apply.
    pub(super) fn record_retained(
        &mut self,
        repository_identity: &str,
        inputs: AuditInputs,
        now: Instant,
    ) {
        if self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(repository_identity) {
            self.entries.clear();
        }
        let retains = match self.entries.get(repository_identity) {
            Some(entry) if entry.inputs == inputs => entry.retains.saturating_add(1),
            _ => 1,
        };
        self.entries.insert(
            repository_identity.to_string(),
            FenceEntry {
                inputs,
                audited_at: now,
                retains,
            },
        );
    }

    /// The cohort was applied, failed to audit or changed shape: remember
    /// nothing, so the next pass audits it.
    pub(super) fn forget(&mut self, repository_identity: &str) {
        self.entries.remove(repository_identity);
    }

    /// Drop every entry whose cohort is no longer listed.
    pub(super) fn retain_listed(&mut self, listed: &HashSet<&str>) {
        self.entries
            .retain(|identity, _| listed.contains(identity.as_str()));
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
    const TIP: &str = "1111111111111111111111111111111111111111";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    fn row(custody: u128, root: &str) -> SourceWorktreeInventoryRow {
        SourceWorktreeInventoryRow {
            agent_scope_disposition: None,
            agent_scope_detail: None,
            custody_id: Uuid::from_u128(custody),
            canonical_repo_dir: "/repo".into(),
            sandbox_root: root.into(),
            sandbox_branch: format!("rsi/{custody}"),
            repository_identity: "repo".into(),
            source_commit: "0".repeat(40),
            owner_session_id: Some(Uuid::from_u128(custody + 100)),
            generation: 1,
            validation_state: "valid".into(),
            validated_generation: 1,
            reserved_effects: 0,
            active_effects: 0,
            session_id: Some(Uuid::from_u128(custody + 100)),
            status: Some("Completed".into()),
            session_updated_at: Some("2026-01-01T00:00:00Z".into()),
            session_working_dir: None,
            session_sandbox_kind: None,
            session_sandbox_root: None,
            session_sandbox_branch: None,
            session_cleanup_state: None,
            session_kind: None,
            pending_archive: false,
            participant_count: 0,
            scheduled_dependency_count: 0,
            scheduled_dependency_digest: String::new(),
            session_path_dependency_count: 0,
            session_path_dependency_digest: String::new(),
            session_path_dependency_reason: None,
            resume_hold: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    fn inputs(rows: Vec<SourceWorktreeInventoryRow>) -> AuditInputs {
        AuditInputs::new(rows, None, TIP, &HashSet::new(), Vec::new())
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
            [10, 10, 20, 30, 30, 30]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn an_unseen_cohort_is_audited_and_a_retained_one_is_fenced_inside_its_window() {
        let start = Instant::now();
        let mut fence = AuditFence::default();
        let inputs = inputs(vec![row(1, "/sb/a")]);
        assert_eq!(fence.decide("repo", &inputs, start), FenceDecision::Audit);
        fence.record_retained("repo", inputs.clone(), start);
        let almost = start + BASE_WINDOW - Duration::from_secs(1);
        assert_eq!(fence.decide("repo", &inputs, almost), FenceDecision::Fenced);
        let due = start + BASE_WINDOW;
        assert_eq!(fence.decide("repo", &inputs, due), FenceDecision::Audit);
        assert_eq!(
            fence.decide("another", &inputs, start),
            FenceDecision::Audit
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn consecutive_unchanged_retentions_widen_the_window_to_the_ceiling_and_the_cap_forces_an_audit()
     {
        let start = Instant::now();
        let mut fence = AuditFence::default();
        let inputs = inputs(vec![row(1, "/sb/a")]);
        let mut now = start;
        for expected in [10, 20, 30, 30, 30] {
            fence.record_retained("repo", inputs.clone(), now);
            let before = now + Duration::from_secs(expected * 60 - 1);
            assert_eq!(fence.decide("repo", &inputs, before), FenceDecision::Fenced);
            now += Duration::from_secs(expected * 60);
            assert_eq!(fence.decide("repo", &inputs, now), FenceDecision::Audit);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn inputs_differing_only_in_the_target_tip_are_unequal_and_audit_at_once() {
        let start = Instant::now();
        let rows = vec![row(1, "/sb/a")];
        let before = AuditInputs::new(rows.clone(), None, TIP, &HashSet::new(), Vec::new());
        let moved = AuditInputs::new(
            rows,
            None,
            "2222222222222222222222222222222222222222",
            &HashSet::new(),
            Vec::new(),
        );
        assert_ne!(before, moved);
        assert_eq!(before, before.clone());
        let mut fence = AuditFence::default();
        fence.record_retained("repo", before.clone(), start);
        assert_eq!(fence.decide("repo", &before, start), FenceDecision::Fenced);
        assert_eq!(fence.decide("repo", &moved, start), FenceDecision::Audit);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_new_removed_or_changed_root_or_run_is_audited_at_once_and_restarts_the_backoff() {
        let start = Instant::now();
        let mut fence = AuditFence::default();
        let base = inputs(vec![row(1, "/sb/a"), row(2, "/sb/b")]);
        fence.record_retained("repo", base.clone(), start);
        fence.record_retained("repo", base.clone(), start);
        let mut changed_row = row(2, "/sb/b");
        changed_row.session_updated_at = Some("2026-01-02T00:00:00Z".into());
        let mut dirty_digest = row(2, "/sb/b");
        dirty_digest.session_path_dependency_digest = "changed".into();
        let with_run = AuditInputs::new(
            base.inventory.clone(),
            Some(Uuid::from_u128(9)),
            TIP,
            &HashSet::new(),
            Vec::new(),
        );
        for different in [
            inputs(vec![row(1, "/sb/a"), row(2, "/sb/b"), row(3, "/sb/c")]),
            inputs(vec![row(1, "/sb/a")]),
            inputs(vec![row(1, "/sb/a"), changed_row]),
            inputs(vec![row(1, "/sb/a"), dirty_digest]),
            with_run,
        ] {
            assert_eq!(
                fence.decide("repo", &different, start),
                FenceDecision::Audit,
                "{different:?}"
            );
        }
        // The inventory order does not matter.
        let reordered = inputs(vec![row(2, "/sb/b"), row(1, "/sb/a")]);
        assert_eq!(
            fence.decide("repo", &reordered, start),
            FenceDecision::Fenced
        );
        // Recording the changed inputs restarts the backoff at the base window.
        let grown = inputs(vec![row(1, "/sb/a"), row(2, "/sb/b"), row(3, "/sb/c")]);
        fence.record_retained("repo", grown.clone(), start);
        let first = start + BASE_WINDOW;
        assert_eq!(
            fence.decide("repo", &grown, first - Duration::from_secs(1)),
            FenceDecision::Fenced
        );
        assert_eq!(fence.decide("repo", &grown, first), FenceDecision::Audit);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_holder_that_can_reach_a_root_changes_the_inputs_and_an_unrelated_session_does_not() {
        let start = Instant::now();
        let mut fence = AuditFence::default();
        let rows = vec![row(1, "/sb/a")];
        let quiet = AuditInputs::new(rows.clone(), None, TIP, &HashSet::new(), Vec::new());
        fence.record_retained("repo", quiet.clone(), start);

        let stranger = Uuid::from_u128(77);
        let unrelated = AuditInputs::new(
            rows.clone(),
            None,
            TIP,
            &HashSet::from([stranger]),
            vec![
                (stranger, "working_dir", PathBuf::from("/elsewhere/project")),
                (stranger, "sandbox_root", PathBuf::from("/sb/other")),
            ],
        );
        assert!(unrelated.holders().is_empty());
        assert_eq!(
            fence.decide("repo", &unrelated, start),
            FenceDecision::Fenced
        );

        // A session working inside the root.
        let inside = AuditInputs::new(
            rows.clone(),
            None,
            TIP,
            &HashSet::from([stranger]),
            vec![(stranger, "working_dir", PathBuf::from("/sb/a/crates"))],
        );
        assert_eq!(inside.holders().len(), 1);
        assert_eq!(fence.decide("repo", &inside, start), FenceDecision::Audit);

        // The root's own owner becoming active.
        let owner = Uuid::from_u128(101);
        let owner_active =
            AuditInputs::new(rows.clone(), None, TIP, &HashSet::from([owner]), Vec::new());
        assert_eq!(
            fence.decide("repo", &owner_active, start),
            FenceDecision::Audit
        );

        // A path that cannot be placed is a holder.
        let relative = AuditInputs::new(
            rows,
            None,
            TIP,
            &HashSet::from([stranger]),
            vec![(stranger, "working_dir", PathBuf::from("relative/dir"))],
        );
        assert_eq!(fence.decide("repo", &relative, start), FenceDecision::Audit);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn forgotten_unlisted_and_cleared_cohorts_are_audited_and_a_zero_window_never_fences() {
        let start = Instant::now();
        let mut fence = AuditFence::default();
        let inputs = inputs(vec![row(1, "/sb/a")]);
        fence.record_retained("a", inputs.clone(), start);
        fence.record_retained("b", inputs.clone(), start);
        fence.record_retained("c", inputs.clone(), start);
        fence.forget("a");
        assert_eq!(fence.decide("a", &inputs, start), FenceDecision::Audit);
        fence.retain_listed(&HashSet::from(["b"]));
        assert_eq!(fence.len(), 1);
        assert_eq!(fence.decide("c", &inputs, start), FenceDecision::Audit);
        assert_eq!(fence.decide("b", &inputs, start), FenceDecision::Fenced);
        fence.clear();
        assert_eq!(fence.len(), 0);

        let mut never = AuditFence::with_windows(Duration::ZERO, Duration::ZERO);
        never.record_retained("a", inputs.clone(), start);
        assert_eq!(never.decide("a", &inputs, start), FenceDecision::Audit);
    }
}
