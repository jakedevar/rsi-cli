//! #582: a typed disk-floor hold on a running `test`/`build` job.
//!
//! A job whose unit has started but whose cargo-slot governor wait is held by
//! the host disk floor compiles nothing, and its log only repeats the wait.
//! The owner (and the worker roster) read it here instead: `held` is
//! `disk_floor` and `hold` carries the free bytes, the floor and how many
//! terminal-worker build caches the reclaim sweep would take. The projection is
//! computed from the live governor when the job is reported and never stored.

use crate::governor::{DiskHeldWaiter, Governor, GovernorPolicy};
use crate::store::Store;
use crate::store::disk_hold_notice::DiskHoldReport;
use chrono::{DateTime, Utc};
use rsi_common::agent_jobs::{
    AgentJobV1, JOB_HELD_FOR_DISK_FLOOR, JOB_HOLD_RECLAIM_COUNT_MAX, JobHoldV1, JobKind, JobState,
};
use rsi_common::agent_worker_roster::{AgentJobSummaryV1, AgentWorkerRosterResultV1};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

const GIB: f64 = 1_073_741_824.0;

/// The governor's disk-held waiters at one moment, with the reclaimable count.
#[derive(Debug, Default)]
pub(crate) struct DiskHoldView {
    waiters: Vec<DiskHeldWaiter>,
    reclaimable_candidates: u32,
}

/// Whether the job can be waiting in the governor queue at all.
fn can_wait_on_floor(job: &AgentJobV1) -> bool {
    job.state == JobState::Running && matches!(job.kind, JobKind::Test | JobKind::Build)
}

impl DiskHoldView {
    #[cfg(test)]
    pub(crate) fn from_waiters(waiters: Vec<DiskHeldWaiter>, reclaimable_candidates: u32) -> Self {
        Self {
            waiters,
            reclaimable_candidates,
        }
    }

    /// Read the live governor, and the reclaim preview only when a waiter is
    /// held. `None` when no job read can be waiting (`any_running_cargo_job`
    /// false) or none is held: the common read pays nothing.
    pub(crate) fn capture(
        store: &Store,
        policy: &GovernorPolicy,
        any_running_cargo_job: bool,
    ) -> Option<Self> {
        if !any_running_cargo_job {
            return None;
        }
        let waiters = Governor::global().disk_held_waiters(policy);
        if waiters.is_empty() {
            return None;
        }
        Some(Self {
            waiters,
            reclaimable_candidates: reclaimable_candidates(store),
        })
    }

    /// The typed hold of `job`, when a held waiter runs in its unit.
    pub(crate) fn hold_of(&self, job: &AgentJobV1) -> Option<JobHoldV1> {
        if !can_wait_on_floor(job) {
            return None;
        }
        self.hold_for_unit(&job.unit_name)
    }

    /// The typed hold of the job running in `unit_name`.
    pub(crate) fn hold_for_unit(&self, unit_name: &str) -> Option<JobHoldV1> {
        let unit = unit_name.trim_end_matches(".service");
        let waiter = self.waiters.iter().find(|waiter| {
            waiter
                .unit
                .as_deref()
                .is_some_and(|candidate| candidate.trim_end_matches(".service") == unit)
        })?;
        Some(JobHoldV1 {
            reason: JOB_HELD_FOR_DISK_FLOOR.to_owned(),
            free_bytes: (waiter.free_gb * GIB).max(0.0) as u64,
            floor_bytes: waiter.min_gb.saturating_mul(1_073_741_824),
            reclaimable_candidates: self.reclaimable_candidates,
        })
    }

    /// Project the hold onto the job's `held` and `hold` fields.
    pub(crate) fn apply(&self, job: &mut AgentJobV1) {
        let Some(hold) = self.hold_of(job) else {
            return;
        };
        job.held = Some(hold.reason.clone());
        job.hold = Some(hold);
    }
}

/// How many terminal-worker build caches the reclaim sweep would take now,
/// bounded by [`JOB_HOLD_RECLAIM_COUNT_MAX`]. A failed preview reads as 0: the
/// hold itself is still true.
fn reclaimable_candidates(store: &Store) -> u32 {
    store
        .preview_terminal_reclaim_page_filtered(JOB_HOLD_RECLAIM_COUNT_MAX, true, true)
        .map_or(0, |page| {
            u32::try_from(page.candidates.len()).unwrap_or(JOB_HOLD_RECLAIM_COUNT_MAX)
        })
}

/// Project the disk hold onto every job of one read (`AgentGetJob`,
/// `AgentListJobs`).
pub(crate) fn project(store: &Store, policy: &GovernorPolicy, jobs: &mut [AgentJobV1]) {
    let Some(view) = DiskHoldView::capture(store, policy, jobs.iter().any(can_wait_on_floor))
    else {
        return;
    };
    for job in jobs {
        view.apply(job);
    }
}

/// Project the disk hold onto the roster's latest-job summaries (#582).
pub(crate) fn project_roster(
    store: &Store,
    policy: &GovernorPolicy,
    roster: &mut AgentWorkerRosterResultV1,
) {
    let running = |summary: &AgentJobSummaryV1| {
        summary.state == JobState::Running && matches!(summary.kind, JobKind::Test | JobKind::Build)
    };
    let any = roster
        .workers
        .iter()
        .filter_map(|worker| worker.latest_job.as_ref())
        .any(running);
    let Some(view) = DiskHoldView::capture(store, policy, any) else {
        return;
    };
    for summary in roster
        .workers
        .iter_mut()
        .filter_map(|worker| worker.latest_job.as_mut())
        .filter(|summary| running(summary))
    {
        summary.hold = view.hold_for_unit(&format!("rsi-job-{}", summary.job_id));
    }
}

/// #644: how long a disk-floor hold must last before its owner and the manager
/// are told, once. Short holds clear on their own and never notify.
pub(crate) const DISK_HOLD_NOTICE_AFTER_SECS: i64 = 3 * 60;

/// How often the watcher looks at the governor. No model call is involved.
const DISK_HOLD_WATCH_TICK: std::time::Duration = std::time::Duration::from_secs(15);

/// The waiter the host disk floor holds in `unit_name`, if any.
fn waiter_in_unit<'a>(
    waiters: &'a [DiskHeldWaiter],
    unit_name: &str,
) -> Option<&'a DiskHeldWaiter> {
    let unit = unit_name.trim_end_matches(".service");
    waiters.iter().find(|waiter| {
        waiter
            .unit
            .as_deref()
            .is_some_and(|candidate| candidate.trim_end_matches(".service") == unit)
    })
}

/// One hold that has lasted past [`DISK_HOLD_NOTICE_AFTER_SECS`] and is not yet
/// notified.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DueHold {
    job_id: Uuid,
    owner_session_id: Uuid,
    held_since: DateTime<Utc>,
    held_secs: u64,
    free_gb: f64,
    min_gb: u64,
}

/// The holds already notified in this process, keyed by job and hold start. An
/// entry lives only while its hold does, so a later hold of the same job is a
/// new key; the store keeps the same key idempotent across a restart.
#[derive(Debug, Default)]
pub(crate) struct DiskHoldWatch {
    notified: HashSet<(Uuid, DateTime<Utc>)>,
}

impl DiskHoldWatch {
    /// The holds of `jobs` to notify now. Forgets holds that cleared.
    pub(crate) fn due(
        &mut self,
        jobs: &[AgentJobV1],
        waiters: &[DiskHeldWaiter],
        now: DateTime<Utc>,
    ) -> Vec<DueHold> {
        let held: Vec<(&AgentJobV1, &DiskHeldWaiter)> = jobs
            .iter()
            .filter(|job| can_wait_on_floor(job))
            .filter_map(|job| Some((job, waiter_in_unit(waiters, &job.unit_name)?)))
            .collect();
        self.notified.retain(|(job_id, since)| {
            held.iter()
                .any(|(job, waiter)| job.id == *job_id && waiter.held_since == *since)
        });
        held.into_iter()
            .filter(|(job, waiter)| !self.notified.contains(&(job.id, waiter.held_since)))
            .filter_map(|(job, waiter)| {
                let secs = (now - waiter.held_since).num_seconds();
                (secs >= DISK_HOLD_NOTICE_AFTER_SECS).then(|| DueHold {
                    job_id: job.id,
                    owner_session_id: job.owner_session_id,
                    held_since: waiter.held_since,
                    held_secs: u64::try_from(secs).unwrap_or(0),
                    free_gb: waiter.free_gb,
                    min_gb: waiter.min_gb,
                })
            })
            .collect()
    }

    fn mark(&mut self, hold: &DueHold) {
        self.notified.insert((hold.job_id, hold.held_since));
    }
}

/// One watcher pass over `jobs` against the live `waiters`: record the notice of
/// each hold that has lasted past the bound. Returns how many holds this pass
/// wrote a new notice for. A failed write is retried by the next pass.
pub(crate) fn notify_persistent_holds(
    store: &Store,
    watch: &mut DiskHoldWatch,
    jobs: &[AgentJobV1],
    waiters: &[DiskHeldWaiter],
    now: DateTime<Utc>,
) -> usize {
    let due = watch.due(jobs, waiters, now);
    if due.is_empty() {
        return 0;
    }
    let reclaimable_candidates = reclaimable_candidates(store);
    let mut written = 0;
    for hold in due {
        let report = DiskHoldReport {
            job_id: hold.job_id,
            owner_session_id: hold.owner_session_id,
            held_since: hold.held_since,
            held_secs: hold.held_secs,
            free_bytes: (hold.free_gb * GIB).max(0.0) as u64,
            floor_bytes: hold.min_gb.saturating_mul(1_073_741_824),
            reclaimable_candidates,
        };
        match store.record_disk_hold_notice(&report) {
            Ok(route) => {
                watch.mark(&hold);
                written += usize::from(route.any());
            }
            Err(error) => {
                tracing::warn!(job = %hold.job_id, %error, "disk hold notice deferred");
            }
        }
    }
    written
}

/// The daemon task (#644): every [`DISK_HOLD_WATCH_TICK`] read the governor and,
/// for each job whose disk-floor hold has lasted past the bound, tell its owner
/// and the manager once. The floor and the job's admitted timeout are untouched.
pub async fn run_disk_hold_watch_loop(
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    config: Arc<crate::config::RuntimeConfig>,
) {
    let mut watch = DiskHoldWatch::default();
    let mut interval = tokio::time::interval(DISK_HOLD_WATCH_TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let policy = config.governor_policy();
        let store = Arc::clone(&store);
        let mut moved = std::mem::take(&mut watch);
        let joined = tokio::task::spawn_blocking(move || {
            let waiters = Governor::global().disk_held_waiters(&policy);
            let guard = crate::store_lock::blocking_lock(&store, "agent_jobs::disk_hold_watch");
            let jobs: Vec<AgentJobV1> = match guard.list_running_agent_jobs() {
                Ok(rows) => rows.into_iter().map(|row| row.job).collect(),
                Err(error) => {
                    tracing::warn!(%error, "disk hold watch cannot list running jobs");
                    return moved;
                }
            };
            let written = notify_persistent_holds(&guard, &mut moved, &jobs, &waiters, Utc::now());
            if written > 0 {
                tracing::info!(written, "disk-floor hold notices recorded");
            }
            moved
        })
        .await;
        match joined {
            Ok(returned) => watch = returned,
            Err(error) => tracing::warn!(%error, "disk hold watch pass failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::JobWake;
    use uuid::Uuid;

    fn job(kind: JobKind, state: JobState, unit: &str) -> AgentJobV1 {
        AgentJobV1 {
            id: Uuid::new_v4(),
            kind,
            name: None,
            state,
            owner_session_id: Uuid::new_v4(),
            unit_name: unit.into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/job.log".into(),
            params: serde_json::from_value(
                serde_json::json!({"kind": "test", "shard": "store-01"}),
            )
            .unwrap(),
            exit_code: None,
            result: None,
            created_at: "2026-10-23T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            held: None,
            hold: None,
            wake: JobWake::Owner,
        }
    }

    fn waiter(unit: Option<&str>) -> DiskHeldWaiter {
        DiskHeldWaiter {
            unit: unit.map(str::to_owned),
            held_since: chrono::Utc::now(),
            free_gb: 17.5,
            min_gb: 30,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_running_job_in_a_disk_held_unit_reads_the_typed_hold() {
        let view = DiskHoldView::from_waiters(
            vec![
                waiter(Some("rsi-job-other.service")),
                waiter(Some("rsi-job-mine.service")),
            ],
            3,
        );
        let mut mine = job(JobKind::Test, JobState::Running, "rsi-job-mine.service");
        view.apply(&mut mine);
        assert_eq!(mine.held.as_deref(), Some("disk_floor"));
        assert_eq!(
            mine.hold,
            Some(JobHoldV1 {
                reason: "disk_floor".into(),
                free_bytes: (17.5 * GIB) as u64,
                floor_bytes: 30 * 1_073_741_824,
                reclaimable_candidates: 3,
            })
        );
        // The roster names the unit without the `.service` suffix: same hold.
        assert_eq!(view.hold_for_unit("rsi-job-mine"), mine.hold);
        assert_eq!(view.hold_for_unit("rsi-job-absent"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn jobs_outside_the_held_units_keep_their_own_state() {
        let view = DiskHoldView::from_waiters(vec![waiter(Some("rsi-job-mine.service"))], 0);
        let mut unrelated = job(
            JobKind::Test,
            JobState::Running,
            "rsi-job-unrelated.service",
        );
        view.apply(&mut unrelated);
        assert_eq!((unrelated.held, unrelated.hold), (None, None));
        // A queued job (held for a deploy) is not in the governor queue; it
        // keeps the reason the store gave it.
        let mut queued = job(JobKind::Test, JobState::Queued, "rsi-job-mine.service");
        queued.held = Some("held_for_deploy".into());
        view.apply(&mut queued);
        assert_eq!(queued.held.as_deref(), Some("held_for_deploy"));
        assert_eq!(queued.hold, None);
    }

    fn waiter_since(unit: &str, held_since: DateTime<Utc>) -> DiskHeldWaiter {
        DiskHeldWaiter {
            held_since,
            ..waiter(Some(unit))
        }
    }

    fn owned_running_job(store: &Store, unit: &str) -> AgentJobV1 {
        let mut session = rsid_store::test_support::make_test_session();
        session.status = rsi_common::types::SessionStatus::Running;
        store.insert_session(&session).unwrap();
        let mut job = job(JobKind::Test, JobState::Running, unit);
        job.owner_session_id = session.id;
        job
    }

    fn mails_to(store: &Store, session: Uuid) -> Vec<String> {
        let mut statement = store
            .conn
            .prepare("SELECT payload FROM agent_messages WHERE target_session_id=?1")
            .unwrap();
        statement
            .query_map([session.to_string()], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_persisting_hold_notifies_its_owner_once_with_the_typed_numbers_644() {
        let store = Store::open_in_memory().unwrap();
        let job = owned_running_job(&store, "rsi-job-held.service");
        let started = Utc::now();
        let waiters = vec![waiter_since("rsi-job-held.service", started)];
        let mut watch = DiskHoldWatch::default();
        let at = |secs: i64| started + chrono::Duration::seconds(secs);
        let jobs = std::slice::from_ref(&job);

        // Under the bound nothing is sent.
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &waiters, at(179)),
            0
        );
        assert!(mails_to(&store, job.owner_session_id).is_empty());

        // Past it the owner is told once, with the job, the free space and floor.
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &waiters, at(181)),
            1
        );
        let mails = mails_to(&store, job.owner_session_id);
        assert_eq!(mails.len(), 1, "{mails:?}");
        for needle in [
            job.id.to_string(),
            "17.5 GiB free".into(),
            "30.0 GiB floor".into(),
        ] {
            assert!(mails[0].contains(&needle), "{needle} missing: {}", mails[0]);
        }

        // Later passes of the same hold, and a fresh watcher (a restart), add none.
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &waiters, at(600)),
            0
        );
        let mut restarted = DiskHoldWatch::default();
        assert_eq!(
            notify_persistent_holds(&store, &mut restarted, jobs, &waiters, at(900)),
            0
        );
        assert_eq!(mails_to(&store, job.owner_session_id).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_hold_that_clears_early_sends_nothing_and_a_second_hold_notifies_again_644() {
        let store = Store::open_in_memory().unwrap();
        let job = owned_running_job(&store, "rsi-job-held.service");
        let jobs = std::slice::from_ref(&job);
        let mut watch = DiskHoldWatch::default();
        let first = Utc::now() - chrono::Duration::minutes(30);

        // Held 2 minutes, then it clears: no waiter, so nothing was ever due.
        let held = vec![waiter_since("rsi-job-held.service", first)];
        let early = first + chrono::Duration::minutes(2);
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &held, early),
            0
        );
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &[], early),
            0
        );
        assert!(mails_to(&store, job.owner_session_id).is_empty());

        // A hold that lasts is notified once...
        let lasting = first + chrono::Duration::minutes(10);
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &held, lasting),
            1
        );
        // ...it clears, and the same job held again (a new start) notifies again.
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &[], lasting),
            0
        );
        let second_start = first + chrono::Duration::minutes(20);
        let second = vec![waiter_since("rsi-job-held.service", second_start)];
        let due = second_start + chrono::Duration::minutes(4);
        assert_eq!(
            notify_persistent_holds(&store, &mut watch, jobs, &second, due),
            1
        );
        assert_eq!(mails_to(&store, job.owner_session_id).len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_the_job_in_the_held_unit_is_due_644() {
        let store = Store::open_in_memory().unwrap();
        let held = owned_running_job(&store, "rsi-job-held.service");
        let other = owned_running_job(&store, "rsi-job-other.service");
        let started = Utc::now() - chrono::Duration::minutes(5);
        let waiters = vec![waiter_since("rsi-job-held.service", started)];
        let mut watch = DiskHoldWatch::default();
        let due = watch.due(&[held.clone(), other], &waiters, Utc::now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].job_id, held.id);
    }
}
