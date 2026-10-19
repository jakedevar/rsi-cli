//! Periodic bounded logical archival of delivered, idle terminal sessions.

use super::SessionManager;
use crate::error::Result;
use crate::store::session_retention::{RetentionGitFacts, RetentionGitNeeds, RetentionPlan};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use uuid::Uuid;

const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// First backoff after a candidate is held (hub #3030, issue #312).
const BASE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Longest a held, unchanged candidate goes without a full evaluation.
const MAX_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Most entries kept. A full map is cleared, which only re-evaluates candidates.
const MAX_ENTRIES: usize = 4096;

/// Hold reasons that say nothing durable about the candidate (it was busy, a
/// probe went stale, the row moved under the check): they are re-evaluated on
/// the next pass.
const UNFENCED_REASONS: &[&str] = &["active", "state_changed", "missing", "probe_stale"];

#[derive(Debug)]
struct FenceEntry {
    fingerprint: String,
    held_at: Instant,
    /// Consecutive evaluations that held this unchanged candidate.
    holds: u32,
}

/// Remembers, in memory, that a candidate was held and what its gate inputs
/// looked like ([`Store::retention_input_fingerprint`]), so an unchanged
/// candidate is not re-evaluated every pass. It only ever re-uses a HELD
/// verdict: a skip cannot archive anything, the archive path always runs every
/// gate (and the IMMEDIATE re-check) in full. The fingerprint includes the
/// effective retention window, bound Issue state, and container descendant
/// statuses alongside the session and hold-table stamps. Elapsed time and
/// external git state are not covered and are rechecked within [`MAX_WINDOW`].
#[derive(Debug)]
pub(crate) struct RetentionFence {
    entries: HashMap<Uuid, FenceEntry>,
    base: Duration,
    max: Duration,
}

impl Default for RetentionFence {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            base: BASE_WINDOW,
            max: MAX_WINDOW,
        }
    }
}

impl RetentionFence {
    /// True when `id`'s inputs are unchanged since a
    /// held evaluation and it is still inside its window.
    fn is_unchanged_hold(&self, id: Uuid, fingerprint: &str, now: Instant) -> bool {
        let Some(entry) = self.entries.get(&id) else {
            return false;
        };
        if entry.fingerprint != fingerprint {
            return false;
        }
        let doublings = entry.holds.saturating_sub(1).min(8);
        let window = self.base.saturating_mul(1_u32 << doublings).min(self.max);
        now.saturating_duration_since(entry.held_at) < window
    }

    fn record_held(&mut self, id: Uuid, fingerprint: String, reason: &str, now: Instant) {
        if UNFENCED_REASONS.contains(&reason) {
            self.entries.remove(&id);
            return;
        }
        if self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(&id) {
            self.entries.clear();
        }
        let holds = match self.entries.get(&id) {
            Some(entry) if entry.fingerprint == fingerprint => entry.holds.saturating_add(1),
            _ => 1,
        };
        self.entries.insert(
            id,
            FenceEntry {
                fingerprint,
                held_at: now,
                holds,
            },
        );
    }

    fn forget(&mut self, id: Uuid) {
        self.entries.remove(&id);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// What phase 1 (under the Store lock) decided for one candidate.
enum Phase1 {
    /// Unchanged since a held evaluation, inside its window: nothing ran.
    Fenced,
    /// Settled without a subprocess.
    Done(Result<Option<&'static str>>),
    /// Every database gate passed; these git facts decide the rest.
    Probe(RetentionGitNeeds),
}

/// The production git probe; tests inject their own.
fn run_git_probe(needs: &RetentionGitNeeds) -> RetentionGitFacts {
    needs.probe()
}

fn retention_ticker() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

pub async fn run_retention_loop(manager: Arc<SessionManager>) {
    let mut interval = retention_ticker();
    let mut after = None;
    let mut fence = RetentionFence::default();
    loop {
        interval.tick().await;
        if let Err(error) = manager.run_retention_pass(&mut after, &mut fence).await {
            tracing::warn!(%error, "Session retention pass deferred");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test(start_paused = true)]
    async fn retention_ticker_runs_on_boot_then_every_ten_minutes() {
        let mut ticker = retention_ticker();
        let first = ticker.tick().await;
        let second = ticker.tick().await;
        assert_eq!(second - first, SWEEP_INTERVAL);
        let third = ticker.tick().await;
        assert_eq!(third - second, SWEEP_INTERVAL);
    }
}

impl SessionManager {
    pub(crate) async fn run_retention_pass(
        &self,
        after: &mut Option<String>,
        fence: &mut RetentionFence,
    ) -> Result<()> {
        self.run_retention_pass_with(after, fence, run_git_probe)
            .await
    }

    /// One bounded pass. A candidate costs the Store lock three times at most
    /// and never across a subprocess: phase 1 reads its facts and evaluates the
    /// database gates, phase 2 runs the git probes on a blocking thread with the
    /// lock released, and phase 3 repeats every gate under one IMMEDIATE
    /// transaction with those facts right before the status change (hub #3030).
    pub(crate) async fn run_retention_pass_with(
        &self,
        after: &mut Option<String>,
        fence: &mut RetentionFence,
        probe: impl Fn(&RetentionGitNeeds) -> RetentionGitFacts + Clone + Send + 'static,
    ) -> Result<()> {
        if !self
            .runtime_config
            .session_retention_enabled
            .load(Ordering::Relaxed)
        {
            fence.clear();
            return Ok(());
        }
        let (candidates, next) = self
            .store
            .lock()
            .await
            .retention_candidates(after.as_deref())?;
        *after = next;
        let mut archived = Vec::new();
        let mut held = BTreeMap::<&'static str, usize>::new();
        // Hub #2940: what this pass costs, summed over its candidates and logged
        // once. The meter covers only the synchronous per-candidate Store work,
        // on the thread that runs it, so another lane's concurrent CPU is
        // never counted; the time spent waiting for the store lock is its own
        // field because every other store caller waits behind that hold.
        let pass_started = Instant::now();
        let candidate_count = candidates.len();
        let mut archive_cost = rsid_core::pass_cost::Cost::default();
        let mut store_lock_wait = Duration::ZERO;
        let mut store_lock_held_max = Duration::ZERO;
        let mut fenced = 0_usize;
        let mut probed = 0_usize;
        let mut probe_wall = Duration::ZERO;
        for id in candidates {
            if !self
                .runtime_config
                .session_retention_enabled
                .load(Ordering::Relaxed)
            {
                *after = None;
                fence.clear();
                break;
            }
            let hours = self
                .runtime_config
                .session_retention_window_hours
                .load(Ordering::Relaxed) as i64;
            let guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
            if self.active.read().await.contains_key(&id) {
                *held.entry("active").or_default() += 1;
                continue;
            }
            let now = Instant::now();
            let lock_requested = Instant::now();
            let (phase1, cost) = {
                let store = crate::store_lock::lock(&self.store, "retention::archive_pass").await;
                store_lock_wait += lock_requested.elapsed();
                rsid_core::pass_cost::ThreadMeter::measure(|| {
                    Self::retention_phase_one(&store, fence, id, hours, now)
                })
            };
            store_lock_held_max = store_lock_held_max.max(cost.wall);
            archive_cost.add(cost);
            let (fingerprint, phase1) = match phase1 {
                Ok(pair) => pair,
                Err(error) => {
                    *held.entry("error").or_default() += 1;
                    tracing::warn!(%id, %error, "Session retention candidate deferred");
                    continue;
                }
            };
            let result = match phase1 {
                Phase1::Fenced => {
                    fenced += 1;
                    tracing::debug!(%id, "Session retention candidate unchanged since a hold");
                    *held.entry("fenced").or_default() += 1;
                    continue;
                }
                Phase1::Done(result) => result,
                Phase1::Probe(needs) => {
                    let probe_started = Instant::now();
                    let probe = probe.clone();
                    let facts = tokio::task::spawn_blocking(move || probe(&needs)).await;
                    probe_wall += probe_started.elapsed();
                    probed += 1;
                    match facts {
                        Err(error) => {
                            *held.entry("error").or_default() += 1;
                            tracing::warn!(%id, %error, "Session retention probe failed");
                            continue;
                        }
                        Ok(facts) => {
                            let lock_requested = Instant::now();
                            let (result, cost) = {
                                let store =
                                    crate::store_lock::lock(&self.store, "retention::archive_pass")
                                        .await;
                                store_lock_wait += lock_requested.elapsed();
                                rsid_core::pass_cost::ThreadMeter::measure(|| {
                                    store.retention_finish_one(id, hours, &facts)
                                })
                            };
                            store_lock_held_max = store_lock_held_max.max(cost.wall);
                            archive_cost.add(cost);
                            result
                        }
                    }
                }
            };
            drop(guard);
            match result {
                Ok(None) => {
                    fence.forget(id);
                    tracing::info!(%id, "Session retention archived");
                    archived.push(id);
                }
                Ok(Some(reason)) => {
                    fence.record_held(id, fingerprint, reason, Instant::now());
                    tracing::debug!(%id, reason, "Session retention held");
                    *held.entry(reason).or_default() += 1;
                }
                Err(error) => {
                    *held.entry("error").or_default() += 1;
                    tracing::warn!(%id, %error, "Session retention candidate deferred");
                }
            }
        }
        self.finish_agent_archive(&archived).await;
        tracing::info!(
            archived = archived.len(),
            ?held,
            candidates = candidate_count,
            fenced,
            probed,
            wall_ms = pass_started.elapsed().as_millis() as u64,
            archive_cpu_ms = archive_cost.cpu_ms(),
            archive_read_kib = archive_cost.read_kib(),
            archive_wall_ms = archive_cost.wall_ms(),
            probe_wall_ms = probe_wall.as_millis() as u64,
            store_lock_wait_ms = store_lock_wait.as_millis() as u64,
            store_lock_held_max_ms = store_lock_held_max.as_millis() as u64,
            "Session retention pass complete"
        );
        Ok(())
    }

    /// Phase 1, under the Store lock and without a subprocess: the fingerprint
    /// of the candidate's gate inputs, the backoff check, and the database
    /// gates. A candidate that needs no git is finished here in the same hold.
    fn retention_phase_one(
        store: &crate::store::Store,
        fence: &RetentionFence,
        id: Uuid,
        hours: i64,
        now: Instant,
    ) -> Result<(String, Phase1)> {
        let fingerprint = store.retention_input_fingerprint(id, hours)?;
        if fence.is_unchanged_hold(id, &fingerprint, now) {
            return Ok((fingerprint, Phase1::Fenced));
        }
        let phase = match store.retention_plan_one(id, hours)? {
            RetentionPlan::Held(reason) => Phase1::Done(Ok(Some(reason))),
            RetentionPlan::Probe(needs) if needs.is_empty() => {
                Phase1::Done(store.retention_finish_one(id, hours, &RetentionGitFacts::default()))
            }
            RetentionPlan::Probe(needs) => Phase1::Probe(needs),
        };
        Ok((fingerprint, phase))
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-session-01")
))]
mod pass_tests {
    use super::*;
    use crate::store::Store;
    use rsi_common::types::SessionStatus;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tempfile::TempDir;

    fn at(base: Instant, minutes: u64) -> Instant {
        base + Duration::from_secs(minutes * 60)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn fence_skips_an_unchanged_hold_inside_its_window_and_nothing_else() {
        let id = Uuid::from_u128(7);
        let base = Instant::now();
        let mut fence = RetentionFence::default();
        assert!(!fence.is_unchanged_hold(id, "a", base), "never held");

        fence.record_held(id, "a".into(), "undelivered", base);
        assert!(fence.is_unchanged_hold(id, "a", at(base, 14)));
        assert!(
            !fence.is_unchanged_hold(id, "b", at(base, 1)),
            "a changed input is re-evaluated at once"
        );
        assert!(
            !fence.is_unchanged_hold(id, "a", at(base, 15)),
            "the window ends"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn fence_window_doubles_per_unchanged_hold_up_to_its_cap() {
        let id = Uuid::from_u128(8);
        let base = Instant::now();
        let mut fence = RetentionFence::default();
        let window = |fence: &RetentionFence, print: &str| {
            (1..=240)
                .find(|m| !fence.is_unchanged_hold(id, print, at(base, *m)))
                .unwrap()
        };
        for expected in [15, 30, 60, 60, 60] {
            fence.record_held(id, "a".into(), "live_worktree", base);
            assert_eq!(window(&fence, "a"), expected);
        }
        // A changed fingerprint restarts the backoff.
        fence.record_held(id, "b".into(), "live_worktree", base);
        assert_eq!(window(&fence, "b"), 15);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn fence_never_remembers_a_transient_hold_and_forgets_an_archive() {
        let id = Uuid::from_u128(9);
        let base = Instant::now();
        let mut fence = RetentionFence::default();
        for reason in UNFENCED_REASONS {
            fence.record_held(id, "a".into(), reason, base);
            assert!(!fence.is_unchanged_hold(id, "a", base), "{reason}");
        }
        fence.record_held(id, "a".into(), "pinned", base);
        assert!(fence.is_unchanged_hold(id, "a", base));
        fence.forget(id);
        assert!(!fence.is_unchanged_hold(id, "a", base));
        fence.record_held(id, "a".into(), "pinned", base);
        fence.clear();
        assert!(!fence.is_unchanged_hold(id, "a", base));
    }

    struct Fx {
        manager: SessionManager,
        dir: TempDir,
    }

    impl Fx {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let sandbox = dir.path().join("sandboxes");
            std::fs::create_dir(&sandbox).unwrap();
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(64)),
                Store::open_test_fixture(&dir.path().join("rsi.db")).unwrap(),
                false,
                dir.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
                sandbox,
            )
            .unwrap();
            Self { manager, dir }
        }

        /// A git repository at `name`; `delivered` puts its HEAD on origin/rolling.
        fn repo(&self, name: &str, delivered: bool) -> std::path::PathBuf {
            let root = self.dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            let git = |args: &[&str]| {
                assert!(
                    Command::new("git")
                        .args([
                            "-c",
                            "user.name=Retention",
                            "-c",
                            "user.email=r@example.invalid"
                        ])
                        .args(args)
                        .current_dir(&root)
                        .status()
                        .unwrap()
                        .success()
                );
            };
            git(&["init", "--quiet"]);
            git(&["commit", "--quiet", "--allow-empty", "-m", "base"]);
            if delivered {
                git(&["update-ref", "refs/remotes/origin/rolling", "HEAD"]);
            }
            root
        }

        /// A finished, idle leaf (no Issue filed) whose live worktree is `root`.
        async fn leaf(&self, root: &std::path::Path) -> Uuid {
            let id = Uuid::new_v4();
            let mut row = rsid_store::test_support::test_session(id, self.dir.path().to_path_buf());
            row.status = SessionStatus::Completed;
            let store = self.manager.store.lock().await;
            store.insert_session(&row).unwrap();
            store
                .conn
                .execute(
                    "UPDATE sessions SET updated_at=?2,created_at=?2,sandbox_kind='GitWorktree',
                       sandbox_root=?3,sandbox_cleanup_state='Live' WHERE id=?1",
                    rusqlite::params![
                        id.to_string(),
                        (chrono::Utc::now() - chrono::Duration::hours(50))
                            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                        root.to_str().unwrap()
                    ],
                )
                .unwrap();
            id
        }

        async fn status(&self, id: Uuid) -> SessionStatus {
            self.manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .unwrap()
                .status
        }
    }

    /// The probe seam: counts calls and records whether the Store lock was free
    /// while the probe ran.
    fn counting_probe(
        manager: &SessionManager,
        calls: &Arc<AtomicUsize>,
        lock_free: &Arc<AtomicBool>,
    ) -> impl Fn(&RetentionGitNeeds) -> RetentionGitFacts + Clone + Send + 'static {
        let store = Arc::clone(&manager.store);
        let (calls, lock_free) = (Arc::clone(calls), Arc::clone(lock_free));
        move |needs| {
            calls.fetch_add(1, Ordering::SeqCst);
            lock_free.store(store.try_lock().is_ok(), Ordering::SeqCst);
            needs.probe()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn git_probes_run_with_no_store_lock_held_and_the_archive_follows() {
        let fx = Fx::new();
        let id = fx.leaf(&fx.repo("delivered", true)).await;
        let (calls, lock_free) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let probe = counting_probe(&fx.manager, &calls, &lock_free);
        let mut after = None;
        let mut fence = RetentionFence::default();
        fx.manager
            .run_retention_pass_with(&mut after, &mut fence, probe)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the candidate was probed");
        assert!(
            lock_free.load(Ordering::SeqCst),
            "the Store lock was free while git ran"
        );
        assert_eq!(fx.status(id).await, SessionStatus::Archived);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_held_candidate_is_skipped_and_a_changed_one_is_reevaluated() {
        let fx = Fx::new();
        let id = fx.leaf(&fx.repo("undelivered", false)).await;
        let (calls, lock_free) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let mut after = None;
        let mut fence = RetentionFence::default();

        for pass in 1..=3 {
            let probe = counting_probe(&fx.manager, &calls, &lock_free);
            fx.manager
                .run_retention_pass_with(&mut after, &mut fence, probe)
                .await
                .unwrap();
            assert_eq!(fx.status(id).await, SessionStatus::Completed, "pass {pass}");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "pass {pass}: the held, unchanged candidate is not probed again"
            );
            after = None;
        }

        // A changed input (the worktree root moved to a delivered checkout) is
        // re-evaluated at once, though its backoff window has not ended.
        let moved = fx.repo("moved", true);
        fx.manager
            .store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE sessions SET sandbox_root=?2 WHERE id=?1",
                rusqlite::params![id.to_string(), moved.to_str().unwrap()],
            )
            .unwrap();
        let probe = counting_probe(&fx.manager, &calls, &lock_free);
        fx.manager
            .run_retention_pass_with(&mut after, &mut fence, probe)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the changed one is probed");
        assert!(lock_free.load(Ordering::SeqCst));
        assert_eq!(fx.status(id).await, SessionStatus::Archived);
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shortening_the_retention_window_rechecks_a_fenced_candidate() {
        let fx = Fx::new();
        let id = fx.leaf(&fx.repo("delivered", true)).await;
        fx.manager
            .runtime_config
            .session_retention_window_hours
            .store(72, Ordering::Relaxed);
        let mut after = None;
        let mut fence = RetentionFence::default();
        fx.manager
            .run_retention_pass(&mut after, &mut fence)
            .await
            .unwrap();
        assert_eq!(fx.status(id).await, SessionStatus::Completed);
        let held = fx
            .manager
            .store
            .lock()
            .await
            .retention_input_fingerprint(id, 72)
            .unwrap();
        assert!(fence.is_unchanged_hold(id, &held, Instant::now()));
        assert!(matches!(
            SessionManager::retention_phase_one(
                &*fx.manager.store.lock().await,
                &fence,
                id,
                72,
                Instant::now()
            )
            .unwrap()
            .1,
            Phase1::Fenced
        ));

        fx.manager
            .runtime_config
            .session_retention_window_hours
            .store(24, Ordering::Relaxed);
        let changed = fx
            .manager
            .store
            .lock()
            .await
            .retention_input_fingerprint(id, 24)
            .unwrap();
        assert_ne!(held, changed);
        assert!(!fence.is_unchanged_hold(id, &changed, Instant::now()));
        after = None;
        fx.manager
            .run_retention_pass(&mut after, &mut fence)
            .await
            .unwrap();
        assert_eq!(fx.status(id).await, SessionStatus::Archived);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn closing_a_bound_issue_rechecks_and_archives_a_fenced_worker() {
        use rsi_common::harness_manager::HarnessManagerConfigV1;
        use rsi_common::harness_manager_v2::{
            ManagerActionStateV2, ManagerActionV2, ManagerLaunchChoiceV2,
        };
        use rsi_common::types::{NewIssue, Project, SessionKind, SessionProvider};

        let fx = Fx::new();
        let id = fx.leaf(&fx.repo("delivered", true)).await;
        let issue = {
            let store = fx.manager.store.lock().await;
            let project = store.get_session(id).unwrap().unwrap().project_id.unwrap();
            let now = chrono::Utc::now();
            store
                .insert_project(&Project {
                    id: project,
                    name: "retention".into(),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            let mut manager =
                rsid_store::test_support::test_session(Uuid::new_v4(), fx.dir.path().to_path_buf());
            manager.status = SessionStatus::Running;
            store.insert_session(&manager).unwrap();
            let issue = store
                .create_issue(&NewIssue {
                    project_id: project,
                    title: "bound retention work".into(),
                    body: String::new(),
                    priority: None,
                    labels: vec![],
                    created_by_session_id: None,
                    assignee: None,
                    idea_id: None,
                    source_event_id: None,
                    source_finding_ref: None,
                })
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE issues SET status='InProgress' WHERE id=?1",
                    [issue.id.to_string()],
                )
                .unwrap();
            let config = HarnessManagerConfigV1 {
                project_id: project,
                manager_session_id: manager.id,
                current_session_id: Some(manager.id),
                epic_ids: vec![],
                scope_mode: Default::default(),
                selected_epic_ids: None,
                group_ids: vec![],
                row_version: 1,
                updated_at: chrono::Utc::now(),
            };
            let operation = store
                .seed_manager_action_with_context_for_test(
                    &config,
                    ManagerActionV2::CreateSession {
                        parent_id: manager.id,
                        kind: SessionKind::Task,
                        query: "bound retention work".into(),
                        launch: ManagerLaunchChoiceV2 {
                            provider: SessionProvider::Codex,
                            model: "test".into(),
                            effort: None,
                        },
                        sandbox_source: None,
                    },
                    ManagerActionStateV2::Succeeded,
                    Some(id),
                    manager.id,
                )
                .unwrap();
            // Admission journals the Issue binding beside the seeded lifecycle
            // payload; keep the real operation/context shape for the fixture.
            store.conn.execute(
                "UPDATE harness_manager_v2_operations SET payload_json=json_set(payload_json,'$.issue_binding',json(?2)) WHERE id=?1",
                rusqlite::params![operation.to_string(), serde_json::json!({"issue_id": issue.id}).to_string()],
            ).unwrap();
            assert_eq!(
                store.automatic_reclaim_resume_hold(id).unwrap(),
                Some("unfinished_issue_worker")
            );
            issue.id
        };
        let mut after = None;
        let mut fence = RetentionFence::default();
        fx.manager
            .run_retention_pass(&mut after, &mut fence)
            .await
            .unwrap();
        assert_eq!(fx.status(id).await, SessionStatus::Completed);
        let held = fx
            .manager
            .store
            .lock()
            .await
            .retention_input_fingerprint(id, 24)
            .unwrap();
        assert!(fence.is_unchanged_hold(id, &held, Instant::now()));
        {
            let store = fx.manager.store.lock().await;
            assert!(matches!(
                SessionManager::retention_phase_one(&store, &fence, id, 24, Instant::now())
                    .unwrap()
                    .1,
                Phase1::Fenced
            ));
            store
                .conn
                .execute(
                    "UPDATE issues SET status='Closed' WHERE id=?1",
                    [issue.to_string()],
                )
                .unwrap();
            let changed = store.retention_input_fingerprint(id, 24).unwrap();
            assert_ne!(held, changed);
            assert!(!fence.is_unchanged_hold(id, &changed, Instant::now()));
        }
        after = None;
        fx.manager
            .run_retention_pass(&mut after, &mut fence)
            .await
            .unwrap();
        assert_eq!(fx.status(id).await, SessionStatus::Archived);
    }
}
