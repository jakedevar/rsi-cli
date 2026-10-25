//! Deploy drain (#1073): while an agent-requested deploy waits for its quiet
//! point, hold the work that would keep it from arriving.
//!
//! The deploy runner ([`crate::deploy::poll_once`]) re-derives the hold from
//! the durable live deploy row every tick, and startup restores it once before
//! any job or merge-queue admission loop starts
//! ([`crate::deploy::start_admission_after_restore`], #2201/#2202), so a
//! restarted daemon never admits work through an empty drain. The hold is
//! bounded by the deploy's own deadline: past it the hold is released whether
//! or not the runner has settled the row yet, and the deploy then settles as
//! refused with its blockers, as before.
//!
//! Only work that starts a *worker* is held: child launches, child
//! continuations, scheduled child wakes and agent jobs. Running turns and
//! running jobs are never interrupted. A parentless operator session and the
//! deploy's own caller are never held.
//!
//! The hold is also bounded by the operator's `deploy_drain_hold_secs`
//! (#1320/#1311): an agent deploy holds new worker starts for at most that long
//! after it was requested. Past it the deploy keeps waiting for a quiet point
//! (`waiting`) without holding worker starts, and engages that hold again from
//! its first quiet poll through the restart. A worker turn can run for an
//! hour, so an unbounded hold starved the manager without reaching quiet.
//!
//! The merge queue is held too (#1128): while a deploy drains, the queue loop
//! admits no new batch (queued entries wait, the running batch finishes, so the
//! `landing_in_progress` quiet-point blocker clears). The held queue shows in
//! `deploy_drain.held` as one `queue_batch` item.
//! Its bounded window starts at the first queue admission opportunity, rather
//! than the deploy request (#1806): an in-flight landing can outlast the worker
//! hold. Each deploy gets one such window per daemon lifetime, capped by the
//! same setting and its deadline. The confirming quiet polls hold both through
//! the restart. While the latest poll names a running local job
//! (`job_running`) the window runs to the deploy's deadline instead (#1996): a
//! job outlasts the cap, and the batch admitted when the cap expired kept the
//! deploy from ever seeing a gap.
//!
//! A staged `interrupt_workers` deploy additionally holds new `test`/`build`
//! jobs for its whole life, past that cap (#2190), so a job started after the worker
//! hold neither piles up behind the restart nor begins just before it.
//! Every other staged deploy holds them to its deadline too, with no retry
//! window (#3373, [`DeployDrain::hold_jobs_until_deadline`]): it stops no job,
//! so it reaches quiet only if running jobs drain, and a job started after the
//! worker hold would keep `job_running` set while the queue sits held.
//! The hold ends when the deploy settles, is cancelled or reaches its deadline;
//! see [`DeployDrain::hold_jobs_until_settled`]. The
//! merge queue likewise admits no new batch for the whole staged deploy. A running
//! ordinary test job is neither stopped nor requeued by such a deploy (#3554):
//! it keeps running across the restart. When such a deploy times
//! out the job hold outlives it for a retry window
//! ([`DeployDrain::hold_jobs_for_retry`]) that starts at the deploy's deadline
//! and is installed with the staged hold, so the hold is continuous (#2551), is
//! owned by the deploy's id (#2553) and is restored from the durable row after a
//! restart (#2552); an immediate retry is not raced.
//!
//! Held work is deferred, never dropped: durable work (spawn requests, topology
//! nodes) waits in place and runs when the hold releases; scheduled wakes stay
//! due and are re-evaluated next tick; work whose caller keeps the request
//! (a manager create, a continuation, a landing or cloud-gate job submit) is
//! answered with the typed refusal [`DEPLOY_DRAINING`] and retried by that
//! caller. A `test` or `build` job submit is held instead (#1566): recorded
//! `queued`, launched by the job loop once the hold ends.

use crate::error::{DaemonError, Result};
use crate::store::agent_deploys::DeployRow;
use chrono::{DateTime, SecondsFormat, Utc};
pub use rsi_common::agent_daemon_info::DEPLOY_DRAINING;
use rsi_common::agent_daemon_info::{DeployDrainV1, HeldWorkV1};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// `hold_cap_secs` value meaning "no cap" (tests and constructors).
const UNCAPPED: u64 = u64::MAX;
use tokio::sync::watch;
use uuid::Uuid;

/// JSON-RPC code of the structured `deploy_draining` refusal.
pub const DEPLOY_DRAINING_RPC_CODE: i32 = -32031;

/// Retry hint (ms) carried by the `deploy_draining` refusal.
const DEPLOY_DRAINING_RETRY_AFTER_MS: u64 = 5_000;

/// The typed, retryable `deploy_draining` refusal: a stable code and an
/// explicit retry action, identical over every transport.
#[must_use]
pub fn draining_error() -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: DEPLOY_DRAINING_RPC_CODE,
        message: DEPLOY_DRAINING.into(),
        data: serde_json::json!({
            "kind": DEPLOY_DRAINING,
            "code": DEPLOY_DRAINING,
            "retryable": true,
            "retry_after_ms": DEPLOY_DRAINING_RETRY_AFTER_MS,
            "next_action": "retry the same request after the deploy settles; AgentGetDaemonInfo deploy_drain.release_by bounds the hold",
        }),
    }
}

/// The message of the refusal every continuation and launch returns once the
/// daemon has decided to restart (#2528). It is a hold, not a failure: nothing
/// ran, so the caller keeps its work and retries after the restart.
pub const RESTART_DRAIN_REFUSAL: &str = "daemon restart drain is in progress";

#[must_use]
pub fn restart_drain_error() -> DaemonError {
    DaemonError::PolicyDenied(RESTART_DRAIN_REFUSAL.into())
}

/// True for the restart-drain refusal ([`restart_drain_error`]).
#[must_use]
pub fn is_restart_drain_error(error: &DaemonError) -> bool {
    matches!(error, DaemonError::PolicyDenied(message) if message == RESTART_DRAIN_REFUSAL)
}

/// True when a delivery was refused because the daemon is draining for a deploy
/// or restarting. The scheduler leaves such a wake untouched: it fires on the
/// next incarnation (#2528) instead of being consumed by the refusal.
#[must_use]
pub fn is_held_for_restart(error: &DaemonError) -> bool {
    is_draining_error(error) || is_restart_drain_error(error)
}

/// True for the `deploy_draining` refusal in either of its shapes.
#[must_use]
pub fn is_draining_error(error: &DaemonError) -> bool {
    match error {
        DaemonError::StructuredRpc { data, .. } => {
            data.get("kind").and_then(serde_json::Value::as_str) == Some(DEPLOY_DRAINING)
        }
        DaemonError::PolicyDenied(message) => message == DEPLOY_DRAINING,
        _ => false,
    }
}

/// What kind of work is parked behind the drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldKind {
    ChildSpawn,
    TopologyNode,
}

impl HeldKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ChildSpawn => "child_spawn",
            Self::TopologyNode => "topology_node",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Active {
    deploy_id: Uuid,
    /// `None` for an operator restart (#1122): every child is held.
    owner: Option<Uuid>,
    deadline: DateTime<Utc>,
}

/// The job hold that outlives a timed-out staged `interrupt_workers` deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryHold {
    /// The deploy that owns the window; only its cancel releases it.
    deploy_id: Uuid,
    /// The deploy's deadline: the window starts where its job hold ends.
    from: DateTime<Utc>,
    until: DateTime<Utc>,
}

/// When the retry hold of a deploy whose deadline is `deadline` ends (#2278,
/// #2551): one fixed boundary after the deadline, derivable from the durable
/// row, so a restart restores it and never extends it (#2552).
#[must_use]
pub(crate) fn retry_hold_until(deadline: DateTime<Utc>) -> DateTime<Utc> {
    deadline
        + chrono::Duration::seconds(
            i64::try_from(rsi_common::agent_deploy::DEPLOY_JOB_RETRY_HOLD_SECS)
                .unwrap_or(i64::MAX / 1000),
        )
}

struct Held {
    ticket: u64,
    kind: HeldKind,
    session_id: Option<Uuid>,
    since: DateTime<Utc>,
}

#[derive(Default)]
struct QueueHold {
    deploy: Option<Active>,
    /// Retained after expiry, so a busy queue cannot renew the same hold.
    since: Option<DateTime<Utc>>,
    has_queued: bool,
}

impl QueueHold {
    /// #2190: the queue admits no new batch for the whole life of a staged
    /// deploy (to its own deadline), so a stream of landings cannot starve it.
    /// The worker-start hold cap (`deploy_drain_hold_secs`) does not bound it.
    fn release_by(&self) -> Option<DateTime<Utc>> {
        self.deploy.map(|deploy| deploy.deadline)
    }
}

/// Process-wide (per `SessionManager`) deploy drain state.
pub struct DeployDrain {
    state: watch::Sender<Option<Active>>,
    waiting: Mutex<Vec<Held>>,
    /// One admission hold window per waiting deploy, starting between batches.
    queue_held: Mutex<QueueHold>,
    /// A staged `interrupt_workers` deploy that holds new test/build jobs for its
    /// whole life, to its deadline, past the worker-start hold cap (#2190).
    jobs_held: Mutex<Option<Active>>,
    /// #2278/#2551: the retry window of a staged `interrupt_workers` deploy,
    /// installed together with its job hold so the hold is continuous from the
    /// staged hold (to the deploy's deadline) into the retry hold (to
    /// [`retry_hold_until`]), so no job is admitted between the two. It keeps new
    /// `test`/`build` jobs held after the deploy timed out, so its retry is not
    /// met by a fresh crop of running jobs. Ends by time, by a new staged
    /// deploy (whose own hold takes over), by [`Self::clear_retry_hold`] when the
    /// deploy settled without timing out, or by [`Self::release`] of this
    /// deploy's id (#2553).
    jobs_retry_hold: Mutex<Option<RetryHold>>,
    /// When a hold on new job starts was last engaged (or the daemon started):
    /// the base of the `job_admission_timed_out` net for a queued job (#2190).
    job_hold_engaged: Mutex<DateTime<Utc>>,
    /// What the waiting deploy's last quiet-point poll found (#1177).
    blockers: Mutex<Vec<&'static str>>,
    /// The deploy waiting for its quiet point and its own deadline, whether
    /// or not it holds anything (#1320).
    waiting_deploy: Mutex<Option<(Uuid, DateTime<Utc>)>>,
    /// Operator `deploy_drain_hold_secs`, refreshed by the deploy runner.
    hold_cap_secs: AtomicU64,
    next_ticket: AtomicU64,
    refused: AtomicU64,
    wakes_held: AtomicU64,
}

impl Default for DeployDrain {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes the waiting entry when the waiter finishes or is cancelled.
struct WaitGuard<'a> {
    drain: &'a DeployDrain,
    ticket: u64,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut waiting) = self.drain.waiting.lock() {
            waiting.retain(|held| held.ticket != self.ticket);
        }
    }
}

impl DeployDrain {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: watch::channel(None).0,
            waiting: Mutex::new(Vec::new()),
            queue_held: Mutex::new(QueueHold::default()),
            jobs_held: Mutex::new(None),
            jobs_retry_hold: Mutex::new(None),
            job_hold_engaged: Mutex::new(Utc::now()),
            blockers: Mutex::new(Vec::new()),
            waiting_deploy: Mutex::new(None),
            hold_cap_secs: AtomicU64::new(UNCAPPED),
            next_ticket: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            wakes_held: AtomicU64::new(0),
        }
    }

    /// Re-derive the hold from the live deploy row: engaged while a deploy is
    /// live (`staged`, or `restarting` until startup verification settles it),
    /// the operator setting is on and the deploy's deadline has not passed.
    /// Waiters are woken the moment the hold ends.
    pub(crate) fn sync(&self, live: Option<&DeployRow>, enabled: bool, now: DateTime<Utc>) {
        self.sync_until(live, None, enabled, now);
    }

    /// [`Self::sync`] with the hold ending at `hold_end` when that comes
    /// before the deploy's deadline (#1320). The deploy keeps `waiting` either
    /// way.
    pub(crate) fn sync_until(
        &self,
        live: Option<&DeployRow>,
        hold_end: Option<DateTime<Utc>>,
        enabled: bool,
        now: DateTime<Utc>,
    ) {
        if let Ok(mut waiting) = self.waiting_deploy.lock() {
            *waiting = live.map(|row| (row.id, row.deadline_at));
        }
        let next = live
            .map(|row| Active {
                deploy_id: row.id,
                owner: row.owner_session_id,
                deadline: hold_end.map_or(row.deadline_at, |end| end.min(row.deadline_at)),
            })
            .filter(|active| enabled && now < active.deadline);
        self.state.send_if_modified(|current| {
            if *current == next {
                false
            } else {
                *current = next;
                true
            }
        });
        let live_id = live.map(|row| row.id);
        if let Ok(mut jobs) = self.jobs_held.lock()
            && jobs.is_some_and(|held| Some(held.deploy_id) != live_id)
        {
            *jobs = None;
        }
        if let Ok(mut queue) = self.queue_held.lock() {
            let deploy = live
                .filter(|row| enabled && now < row.deadline_at)
                .map(|row| Active {
                    deploy_id: row.id,
                    owner: row.owner_session_id,
                    deadline: row.deadline_at,
                });
            if queue.deploy.map(|d| d.deploy_id) != deploy.map(|d| d.deploy_id) {
                queue.since = None;
                queue.has_queued = false;
            }
            queue.deploy = deploy;
        }
    }

    /// Release the hold of deploy `deploy_id` at once (its owner cancelled
    /// it, #1320). Another deploy's hold is left alone.
    pub(crate) fn release(&self, deploy_id: Uuid) {
        self.state.send_if_modified(|current| {
            if current.is_some_and(|active| active.deploy_id == deploy_id) {
                *current = None;
                true
            } else {
                false
            }
        });
        if let Ok(mut waiting) = self.waiting_deploy.lock()
            && waiting.is_some_and(|(id, _)| id == deploy_id)
        {
            *waiting = None;
        }
        if let Ok(mut jobs) = self.jobs_held.lock()
            && jobs.is_some_and(|held| held.deploy_id == deploy_id)
        {
            *jobs = None;
        }
        self.clear_retry_hold(deploy_id);
        if let Ok(mut queue) = self.queue_held.lock()
            && queue
                .deploy
                .is_some_and(|deploy| deploy.deploy_id == deploy_id)
        {
            *queue = QueueHold::default();
        }
    }

    /// Set the operator's hold cap (`deploy_drain_hold_secs`).
    pub(crate) fn set_hold_cap_secs(&self, secs: u64) {
        self.hold_cap_secs.store(secs, Ordering::Relaxed);
    }

    /// The hold cap, `None` when uncapped.
    pub(crate) fn hold_cap(&self) -> Option<chrono::Duration> {
        let secs = self.hold_cap_secs.load(Ordering::Relaxed);
        (secs != UNCAPPED)
            .then(|| chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX / 1000)))
    }

    /// The hold's release time for the deploy `id`, when it holds (#1311).
    #[must_use]
    pub fn release_by(&self) -> Option<(Uuid, DateTime<Utc>)> {
        self.active_at(Utc::now())
            .map(|active| (active.deploy_id, active.deadline))
    }

    /// Record the quiet-point blockers the latest poll found (empty clears).
    pub(crate) fn set_blockers(&self, blockers: &[&'static str]) {
        if let Ok(mut current) = self.blockers.lock() {
            current.clear();
            current.extend_from_slice(blockers);
        }
    }

    /// #2190: hold new `test`/`build` job starts for the whole life of `row`
    /// (to its deadline), whether or not the worker-start hold is still engaged.
    /// For a staged `interrupt_workers` deploy only: a job admitted meanwhile
    /// would start just before the restart it survives (#3554). Cleared
    /// when the deploy settles ([`Self::sync_until`]) or is cancelled
    /// ([`Self::release`]).
    pub(crate) fn hold_jobs_until_settled(&self, row: &DeployRow, now: DateTime<Utc>) {
        // #2551: the retry window is installed with the staged hold, so the
        // hold stays continuous across the deadline even before the runner has
        // settled the timeout. It replaces an older deploy's window.
        self.hold_jobs_for_retry(row);
        self.hold_jobs_until_deadline(row, now);
    }

    /// #3373: hold new `test`/`build` job starts to the deadline of the staged
    /// deploy `row` with no retry window. A deploy that does not interrupt
    /// workers never stops a running job, so it can reach its quiet point only
    /// if running jobs drain; a job started after the worker-start hold would
    /// keep `job_running` set to the deadline, hold the merge queue the whole
    /// time and time the deploy out. Cleared like
    /// [`Self::hold_jobs_until_settled`].
    pub(crate) fn hold_jobs_until_deadline(&self, row: &DeployRow, now: DateTime<Utc>) {
        if let Ok(mut jobs) = self.jobs_held.lock() {
            *jobs = (now < row.deadline_at).then_some(Active {
                deploy_id: row.id,
                owner: row.owner_session_id,
                deadline: row.deadline_at,
            });
        }
    }

    /// #2278: keep new `test`/`build` job starts held from the deadline of the
    /// staged `interrupt_workers` deploy `row` to [`retry_hold_until`] (its job
    /// hold ends at the deadline, and a timed-out deploy would otherwise start
    /// every held job at once). A retry staged inside the window takes the hold
    /// over ([`Self::hold_jobs_until_settled`]). The startup restore installs
    /// the same window from a timed-out row (#2552).
    pub(crate) fn hold_jobs_for_retry(&self, row: &DeployRow) {
        if let Ok(mut retry) = self.jobs_retry_hold.lock() {
            *retry = Some(RetryHold {
                deploy_id: row.id,
                from: row.deadline_at,
                until: retry_hold_until(row.deadline_at),
            });
        }
    }

    /// The deploy that owns the retry window, if one is installed.
    pub(crate) fn retry_hold_owner(&self) -> Option<Uuid> {
        self.jobs_retry_hold
            .lock()
            .ok()
            .and_then(|retry| *retry)
            .map(|retry| retry.deploy_id)
    }

    /// Drop the retry window of `deploy_id` (it settled without timing out, or
    /// was cancelled). Another deploy's window is left alone (#2553).
    pub(crate) fn clear_retry_hold(&self, deploy_id: Uuid) {
        if let Ok(mut retry) = self.jobs_retry_hold.lock()
            && retry.is_some_and(|hold| hold.deploy_id == deploy_id)
        {
            *retry = None;
        }
    }

    fn jobs_retry_hold_at(&self, now: DateTime<Utc>) -> Option<RetryHold> {
        self.jobs_retry_hold
            .lock()
            .ok()
            .and_then(|retry| *retry)
            .filter(|hold| now < hold.until)
    }

    fn jobs_held_at(&self, now: DateTime<Utc>) -> Option<Active> {
        self.jobs_held
            .lock()
            .ok()
            .and_then(|jobs| *jobs)
            .filter(|held| now < held.deadline)
    }

    /// True when a new `test`/`build` job must wait instead of starting: the
    /// worker-start hold applies to `session` ([`Self::holds`]), or a staged
    /// `interrupt_workers` deploy holds every job start, the deploy's own caller
    /// and parentless sessions included, since the deploy would stop theirs too.
    #[must_use]
    pub fn holds_job_start(&self, session: Option<Uuid>, has_parent: bool) -> bool {
        self.holds_job_start_at(session, has_parent, Utc::now())
    }

    pub(crate) fn holds_job_start_at(
        &self,
        session: Option<Uuid>,
        has_parent: bool,
        now: DateTime<Utc>,
    ) -> bool {
        let held = self.jobs_held_at(now).is_some()
            || self.jobs_retry_hold_at(now).is_some()
            || self.holds_at(session, has_parent, now);
        if held {
            self.note_job_hold_engaged(now);
        }
        held
    }

    /// Record `at` as the last time a hold on new job starts was engaged.
    pub(crate) fn note_job_hold_engaged(&self, at: DateTime<Utc>) {
        if let Ok(mut engaged) = self.job_hold_engaged.lock() {
            *engaged = at;
        }
    }

    /// When a hold on new job starts was last engaged, or the drain was created
    /// (the daemon started) if never. A queued job's admission clock runs from
    /// the later of its submission and this, so a long deploy hold, and a daemon
    /// restart through it, never count against the job (#2190).
    pub(crate) fn job_hold_last_engaged(&self) -> DateTime<Utc> {
        self.job_hold_engaged
            .lock()
            .map_or_else(|_| Utc::now(), |engaged| *engaged)
    }

    /// When the hold on a new job's start ends at the latest: the later of the
    /// worker-start hold and the staged-deploy job hold (the deploy's deadline).
    #[must_use]
    pub fn job_release_by(&self) -> Option<DateTime<Utc>> {
        let now = Utc::now();
        self.active_at(now)
            .map(|active| active.deadline)
            .into_iter()
            .chain(self.jobs_held_at(now).map(|held| held.deadline))
            // The window is a promise only once the deploy's deadline has passed.
            .chain(
                self.jobs_retry_hold_at(now)
                    .filter(|hold| now >= hold.from)
                    .map(|hold| hold.until),
            )
            .max()
    }

    fn active_at(&self, now: DateTime<Utc>) -> Option<Active> {
        (*self.state.borrow()).filter(|active| now < active.deadline)
    }

    /// True while a hold is engaged (deadline not yet passed).
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.active_at(Utc::now()).is_some()
    }

    /// True when `session` must be held: a hold is engaged and the session is
    /// neither the deploy's caller nor parentless (`has_parent == false`).
    #[must_use]
    pub fn holds(&self, session: Option<Uuid>, has_parent: bool) -> bool {
        self.holds_at(session, has_parent, Utc::now())
    }

    pub(crate) fn holds_at(
        &self,
        session: Option<Uuid>,
        has_parent: bool,
        now: DateTime<Utc>,
    ) -> bool {
        self.active_at(now).is_some_and(|active| {
            has_parent && active.owner.is_none_or(|owner| session != Some(owner))
        })
    }

    /// Refuse with the typed `deploy_draining` code when `session` is held.
    /// For work whose caller keeps the request and retries.
    ///
    /// # Errors
    /// `deploy_draining`.
    pub fn refuse_if_draining(&self, session: Option<Uuid>, has_parent: bool) -> Result<()> {
        if self.holds(session, has_parent) {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(draining_error());
        }
        Ok(())
    }

    /// Merge-queue admission gate (#1128): `true` when the queue must not admit
    /// a new batch. The window starts here, after any in-flight batch finishes
    /// (#1806), even if the hold on worker starts has already expired.
    /// `has_queued` records (or clears) the held item reported by [`Self::status`].
    #[must_use]
    pub fn hold_queue_admission(&self, has_queued: bool) -> bool {
        self.hold_queue_admission_at(has_queued, Utc::now())
    }

    fn hold_queue_admission_at(&self, has_queued: bool, now: DateTime<Utc>) -> bool {
        let active = self.active_at(now);
        if let Ok(mut queue) = self.queue_held.lock() {
            queue.has_queued = has_queued;
            if queue.deploy.is_some_and(|d| now < d.deadline) {
                if has_queued {
                    queue.since.get_or_insert(now);
                }
                return true;
            }
        }
        active.is_some()
    }

    /// #3885: the staged deploy that holds merge-queue admission right now, if
    /// any, for `AgentListRollingQueue`'s `held_for_deploy`.
    #[must_use]
    pub fn queue_deploy_hold(&self) -> Option<rsi_common::rolling_queue::QueueDeployHoldV1> {
        self.queue_deploy_hold_at(Utc::now())
    }

    fn queue_deploy_hold_at(
        &self,
        now: DateTime<Utc>,
    ) -> Option<rsi_common::rolling_queue::QueueDeployHoldV1> {
        let queue = self.queue_held.lock().ok()?;
        let deploy = queue.deploy.filter(|d| now < d.deadline)?;
        Some(rsi_common::rolling_queue::QueueDeployHoldV1 {
            deploy_id: deploy.deploy_id,
            release_by: deploy.deadline.to_rfc3339_opts(SecondsFormat::Nanos, true),
        })
    }

    /// Note a queued manager action that was returned to the queue.
    pub fn note_manager_action_held(&self) {
        self.wakes_held.fetch_add(1, Ordering::Relaxed);
    }

    /// Note a scheduled resume wake that was left due instead of delivered.
    pub fn note_wake_held(&self) {
        self.wakes_held.fetch_add(1, Ordering::Relaxed);
    }

    /// Wait, in place, until the hold releases. Returns at once when the work
    /// is not held. Bounded by the deploy's deadline even if the runner stalls.
    /// Returns `true` when it actually waited.
    pub async fn wait_released(
        &self,
        kind: HeldKind,
        session_id: Option<Uuid>,
        has_parent: bool,
    ) -> bool {
        let now = Utc::now();
        if !self.holds_at(session_id, has_parent, now) {
            return false;
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.push(Held {
                ticket,
                kind,
                session_id,
                since: now,
            });
        }
        let _guard = WaitGuard {
            drain: self,
            ticket,
        };
        let mut rx = self.state.subscribe();
        loop {
            let Some(active) = self.active_at(Utc::now()) else {
                return true;
            };
            let remaining = (active.deadline - Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::ZERO);
            match tokio::time::timeout(remaining, rx.changed()).await {
                Ok(Ok(())) => {}
                // Deadline reached or the sender is gone: release.
                Ok(Err(_)) | Err(_) => return true,
            }
        }
    }

    /// Operator/agent-visible status (`AgentGetDaemonInfo`).
    #[must_use]
    pub fn status(&self) -> DeployDrainV1 {
        self.status_at(Utc::now())
    }

    fn status_at(&self, now: DateTime<Utc>) -> DeployDrainV1 {
        let active = self.active_at(now);
        let held = self.waiting.lock().map_or_else(
            |_| Vec::new(),
            |waiting| {
                waiting
                    .iter()
                    .map(|entry| HeldWorkV1 {
                        kind: entry.kind.as_str().to_string(),
                        session_id: entry.session_id,
                        since: entry.since.to_rfc3339_opts(SecondsFormat::Nanos, true),
                        reason: DEPLOY_DRAINING.to_string(),
                    })
                    .collect()
            },
        );
        let mut held = held;
        let queue_hold = self.queue_held.lock().ok().and_then(|queue| {
            let end = queue.release_by()?;
            (queue.has_queued && now < end).then_some((queue.since?, end))
        });
        if let Some((since, _)) = queue_hold {
            held.push(HeldWorkV1 {
                kind: "queue_batch".to_string(),
                session_id: None,
                since: since.to_rfc3339_opts(SecondsFormat::Nanos, true),
                reason: DEPLOY_DRAINING.to_string(),
            });
        }
        let waiting = self
            .waiting_deploy
            .lock()
            .ok()
            .and_then(|waiting| *waiting)
            .filter(|(_, deadline)| now < *deadline);
        let jobs_held = self.jobs_held_at(now);
        let release_by = active
            .map(|value| value.deadline)
            .into_iter()
            .chain(queue_hold.map(|(_, end)| end))
            .chain(jobs_held.map(|value| value.deadline))
            .max();
        DeployDrainV1 {
            draining: release_by.is_some(),
            deploy_id: active
                .or(jobs_held)
                .map(|value| value.deploy_id)
                .or(waiting.map(|(id, _)| id)),
            release_by: release_by.map(|end| end.to_rfc3339_opts(SecondsFormat::Nanos, true)),
            waiting: waiting.is_some(),
            deploy_deadline_at: waiting
                .map(|(_, deadline)| deadline.to_rfc3339_opts(SecondsFormat::Nanos, true)),
            held,
            blockers: if active.is_some() || waiting.is_some() {
                self.blockers.lock().map_or_else(
                    |_| Vec::new(),
                    |b| b.iter().map(|b| (*b).to_string()).collect(),
                )
            } else {
                Vec::new()
            },
            refused_total: self.refused.load(Ordering::Relaxed),
            wakes_held_total: self.wakes_held.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_deploy::DeployState;
    use std::sync::Arc;

    fn row(owner: Uuid, deadline: DateTime<Utc>) -> DeployRow {
        DeployRow {
            id: Uuid::new_v4(),
            owner_session_id: Some(owner),
            sha: "0".repeat(40),
            manifest: Vec::new(),
            state: DeployState::Staged,
            reason: None,
            deadline_at: deadline,
            operator: false,
            forced: false,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_refusal_is_structured_with_a_stable_code_and_a_retry_action() {
        let drain = DeployDrain::new();
        let owner = Uuid::new_v4();
        let worker = Uuid::new_v4();
        drain.sync(
            Some(&row(owner, Utc::now() + chrono::Duration::seconds(60))),
            true,
            Utc::now(),
        );
        let error = drain.refuse_if_draining(Some(worker), true).unwrap_err();
        assert!(is_draining_error(&error));
        let DaemonError::StructuredRpc {
            rpc_code,
            message,
            data,
        } = &error
        else {
            panic!("the refusal is a structured RPC error: {error}");
        };
        assert_eq!(*rpc_code, DEPLOY_DRAINING_RPC_CODE);
        assert_eq!(message, DEPLOY_DRAINING);
        assert_eq!(data["code"], DEPLOY_DRAINING);
        assert_eq!(data["retryable"], true);
        assert!(data["retry_after_ms"].as_u64().is_some_and(|ms| ms > 0));
        assert!(
            data["next_action"]
                .as_str()
                .is_some_and(|action| action.contains("retry"))
        );
        assert!(!is_draining_error(&DaemonError::PolicyDenied(
            "other".into()
        )));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn parentless_sessions_and_the_deploy_caller_are_never_held() {
        let drain = DeployDrain::new();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        drain.sync(
            Some(&row(owner, now + chrono::Duration::seconds(60))),
            true,
            now,
        );
        let worker = Uuid::new_v4();
        assert!(drain.holds(Some(worker), true));
        assert!(drain.holds(None, true), "a new launch has no id yet");
        assert!(
            !drain.holds(Some(worker), false),
            "parentless operator session"
        );
        assert!(!drain.holds(Some(owner), true), "the deploy's caller");
        assert!(drain.refuse_if_draining(Some(owner), true).is_ok());
        let error = drain
            .refuse_if_draining(Some(worker), true)
            .expect_err("held");
        assert!(error.to_string().contains(DEPLOY_DRAINING));
        assert_eq!(drain.status().refused_total, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_setting_off_or_a_passed_deadline_holds_nothing() {
        let drain = DeployDrain::new();
        let now = Utc::now();
        let live = row(Uuid::new_v4(), now + chrono::Duration::seconds(60));
        drain.sync(Some(&live), false, now);
        assert!(!drain.is_draining());
        drain.sync(Some(&live), true, now);
        assert!(drain.is_draining());
        drain.sync(Some(&live), true, now + chrono::Duration::seconds(61));
        assert!(!drain.is_draining());
        drain.sync(Some(&live), true, now);
        drain.sync(None, true, now);
        assert!(!drain.status().draining);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn a_waiter_runs_when_the_hold_releases_and_is_listed_meanwhile() {
        let drain = Arc::new(DeployDrain::new());
        let now = Utc::now();
        drain.sync(
            Some(&row(Uuid::new_v4(), now + chrono::Duration::seconds(60))),
            true,
            now,
        );
        let child = Uuid::new_v4();
        let waiter = {
            let drain = Arc::clone(&drain);
            tokio::spawn(async move {
                drain
                    .wait_released(HeldKind::ChildSpawn, Some(child), true)
                    .await
            })
        };
        for _ in 0..200 {
            if !drain.status().held.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let status = drain.status();
        assert_eq!(status.held.len(), 1);
        assert_eq!(status.held[0].session_id, Some(child));
        assert_eq!(status.held[0].reason, DEPLOY_DRAINING);
        assert!(!waiter.is_finished());
        drain.sync(None, true, Utc::now());
        assert!(waiter.await.expect("join"), "the waiter waited, then ran");
        assert!(drain.status().held.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[tokio::test]
    async fn the_hold_is_bounded_by_the_deploy_deadline_even_if_the_runner_stalls() {
        let drain = DeployDrain::new();
        let now = Utc::now();
        drain.sync(
            Some(&row(
                Uuid::new_v4(),
                now + chrono::Duration::milliseconds(150),
            )),
            true,
            now,
        );
        let started = std::time::Instant::now();
        let waited = drain
            .wait_released(HeldKind::TopologyNode, None, true)
            .await;
        assert!(waited);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!drain.is_draining());
        assert!(
            !drain
                .wait_released(HeldKind::ChildSpawn, Some(Uuid::new_v4()), true)
                .await,
            "after release nothing waits"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_queue_hold_is_reported_in_held_only_while_draining_with_queued_entries() {
        let drain = DeployDrain::new();
        assert!(!drain.hold_queue_admission(true), "no deploy: admit");
        assert!(drain.status().held.is_empty());
        let now = Utc::now();
        let live = row(Uuid::new_v4(), now + chrono::Duration::minutes(5));
        drain.sync(Some(&live), true, now);
        assert!(
            drain.hold_queue_admission(false),
            "draining holds admission"
        );
        assert!(
            drain.status().held.is_empty(),
            "nothing queued, nothing held"
        );
        assert!(drain.hold_queue_admission(true));
        let held = drain.status().held;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].kind, "queue_batch");
        assert_eq!(held[0].reason, DEPLOY_DRAINING);
        drain.sync(None, true, now);
        assert!(!drain.hold_queue_admission(true));
        assert!(drain.status().held.is_empty());
    }

    /// #2190: while a deploy is staged the queue admits no NEW batch, to the
    /// deploy's own deadline, however long the worker-start hold cap (and the
    /// deploy's wait) is; settling or abandoning the deploy releases it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_queue_admits_no_new_batch_while_a_deploy_is_staged_past_the_worker_hold_cap() {
        let drain = DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let now = Utc::now();
        let live = row(Uuid::new_v4(), now + chrono::Duration::minutes(30));
        let worker_end = Some(now - chrono::Duration::seconds(1));
        drain.sync_until(Some(&live), worker_end, true, now);
        assert!(!drain.is_draining(), "the capped worker hold is over");
        let end = live.deadline_at;

        // The queue is held at the first opportunity and every one after it,
        // with or without queued entries, however long the deploy waits.
        assert!(drain.hold_queue_admission_at(true, now));
        for minutes in [1, 2, 10, 29] {
            let later = now + chrono::Duration::minutes(minutes);
            drain.sync_until(Some(&live), worker_end, true, later);
            assert!(drain.hold_queue_admission_at(false, later), "{minutes} min");
            assert!(drain.hold_queue_admission_at(true, later), "{minutes} min");
        }
        let status = drain.status_at(now + chrono::Duration::minutes(29));
        assert!(status.draining);
        assert_eq!(status.held[0].kind, "queue_batch");
        assert_eq!(
            status.held[0].since,
            now.to_rfc3339_opts(SecondsFormat::Nanos, true)
        );
        assert_eq!(
            status.release_by,
            Some(end.to_rfc3339_opts(SecondsFormat::Nanos, true))
        );

        // The deploy's deadline ends the hold, as does abandoning the deploy.
        assert!(!drain.hold_queue_admission_at(true, end));
        let expired = drain.status_at(end);
        assert!(!expired.draining);
        assert!(expired.held.is_empty());
        assert!(expired.release_by.is_none());
        drain.sync_until(Some(&live), worker_end, true, now);
        assert!(drain.hold_queue_admission_at(true, now));
        drain.release(live.id);
        assert!(!drain.hold_queue_admission_at(true, now + chrono::Duration::seconds(1)));
        assert!(drain.status().held.is_empty());

        // A settled deploy (no live row) releases it too.
        drain.sync_until(Some(&live), worker_end, true, now);
        assert!(drain.hold_queue_admission_at(true, now));
        drain.sync(None, true, now);
        assert!(!drain.hold_queue_admission_at(true, now));
    }

    /// #3885: the queue listing names the deploy that holds admission, to its
    /// deadline, however long the worker-start hold cap is; a settled,
    /// cancelled or expired deploy (and a daemon with no deploy, such as the
    /// one a peer deploy leaves alone) shows no hold.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_queue_listing_names_the_staged_deploy_that_holds_admission() {
        let drain = DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let now = Utc::now();
        assert!(drain.queue_deploy_hold_at(now).is_none(), "no hub deploy");
        let live = row(Uuid::new_v4(), now + chrono::Duration::minutes(30));
        let worker_end = Some(now - chrono::Duration::seconds(1));
        drain.sync_until(Some(&live), worker_end, true, now);
        let later = now + chrono::Duration::minutes(10);
        let hold = drain
            .queue_deploy_hold_at(later)
            .expect("held past the cap");
        assert_eq!(hold.deploy_id, live.id);
        assert_eq!(
            hold.release_by,
            live.deadline_at.to_rfc3339_opts(SecondsFormat::Nanos, true)
        );
        assert!(drain.queue_deploy_hold_at(live.deadline_at).is_none());
        drain.release(live.id);
        assert!(drain.queue_deploy_hold_at(now).is_none(), "cancelled");
        drain.sync_until(Some(&live), worker_end, true, now);
        drain.sync(None, true, now);
        assert!(drain.queue_deploy_hold_at(now).is_none(), "settled");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_queue_window_respects_deadlines_disabled_drain_and_deploy_identity() {
        let drain = DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let now = Utc::now();
        let live = row(Uuid::new_v4(), now + chrono::Duration::seconds(20));
        let worker_end = Some(now - chrono::Duration::seconds(1));
        drain.sync_until(Some(&live), worker_end, false, now);
        assert!(!drain.hold_queue_admission_at(true, now));
        drain.sync_until(Some(&live), worker_end, true, now);
        assert!(drain.hold_queue_admission_at(true, now));
        assert_eq!(
            drain.status().release_by,
            Some(live.deadline_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
        );
        assert!(!drain.hold_queue_admission_at(true, live.deadline_at));
        assert!(!drain.status_at(live.deadline_at).draining);
        assert!(drain.status_at(live.deadline_at).held.is_empty());

        let next = row(Uuid::new_v4(), now + chrono::Duration::minutes(5));
        drain.sync_until(Some(&next), worker_end, true, now);
        assert!(drain.hold_queue_admission_at(true, now));
        drain.release(live.id);
        assert!(drain.hold_queue_admission_at(true, now));
        drain.sync_until(Some(&next), worker_end, false, now);
        assert!(!drain.hold_queue_admission_at(true, now));
        assert!(drain.status().held.is_empty());
    }

    /// #3373: the deadline-only job hold (a deploy that stops no job) ends at
    /// the deadline with no retry window and is dropped by a settle or cancel.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_deadline_job_hold_has_no_retry_window() {
        let drain = DeployDrain::new();
        let worker = Uuid::new_v4();
        let now = Utc::now();
        let live = row(Uuid::new_v4(), now + chrono::Duration::minutes(30));
        drain.hold_jobs_until_deadline(&live, now);
        assert!(drain.holds_job_start(Some(worker), true));
        assert!(drain.holds_job_start(None, false));
        assert_eq!(drain.job_release_by(), Some(live.deadline_at));
        assert!(!drain.holds_job_start_at(Some(worker), true, live.deadline_at));
        drain.hold_jobs_until_deadline(&live, live.deadline_at);
        assert!(drain.jobs_held_at(live.deadline_at).is_none());
        drain.hold_jobs_until_deadline(&live, now);
        drain.sync(None, true, now);
        assert!(!drain.holds_job_start(Some(worker), true), "settled");
        drain.hold_jobs_until_deadline(&live, now);
        drain.release(live.id);
        assert!(!drain.holds_job_start(Some(worker), true), "cancelled");
    }

    /// #2190: a staged `interrupt_workers` deploy holds `test`/`build` job
    /// starts to its deadline, past the worker-start hold cap, for everyone
    /// (its own caller and parentless sessions included), and drops the hold
    /// the moment it settles, is cancelled, or another deploy takes over.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_staged_interrupt_deploy_holds_job_starts_to_its_deadline_past_the_cap() {
        let drain = DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let owner = Uuid::new_v4();
        let worker = Uuid::new_v4();
        let now = Utc::now();
        let live = row(owner, now + chrono::Duration::minutes(30));
        let worker_end = Some(now - chrono::Duration::seconds(1));
        drain.sync_until(Some(&live), worker_end, true, now);
        assert!(
            !drain.holds_job_start(Some(worker), true),
            "cap over, no job hold yet"
        );
        assert!(drain.job_release_by().is_none());

        drain.hold_jobs_until_settled(&live, now);
        for session in [Some(worker), Some(owner), None] {
            for has_parent in [true, false] {
                assert!(drain.holds_job_start(session, has_parent));
            }
        }
        assert_eq!(drain.job_release_by(), Some(live.deadline_at));
        let status = drain.status();
        assert!(status.draining, "{status:?}");
        assert_eq!(status.deploy_id, Some(live.id));
        assert_eq!(
            status.release_by,
            Some(live.deadline_at.to_rfc3339_opts(SecondsFormat::Nanos, true))
        );
        // A deploy poll's resync keeps the job hold; the worker hold stays over.
        drain.sync_until(Some(&live), worker_end, true, now);
        assert!(drain.holds_job_start(Some(worker), true));
        assert!(!drain.holds(Some(worker), true));
        // Child launches and wakes keep using the capped hold, not this one.
        assert!(drain.refuse_if_draining(Some(worker), true).is_ok());

        // The deadline ends the staged hold even if the runner stalls; the retry
        // window (#2551) then carries the hold to its own end.
        drain.hold_jobs_until_settled(&live, live.deadline_at);
        assert!(drain.jobs_held_at(live.deadline_at).is_none());
        assert!(drain.holds_job_start_at(Some(worker), true, live.deadline_at));
        assert!(!drain.holds_job_start_at(Some(worker), true, retry_hold_until(live.deadline_at)));
        drain.hold_jobs_until_settled(&live, now);
        assert!(drain.holds_job_start(Some(worker), true));

        // Another deploy taking the slot drops this one's hold.
        let next = row(owner, now + chrono::Duration::minutes(5));
        drain.sync_until(Some(&next), worker_end, true, now);
        assert!(drain.jobs_held_at(now).is_none());
        // Its retry window outlives the slot change until the runner's reconcile
        // finds that deploy settled without timing out (`clear_retry_hold`).
        assert!(drain.holds_job_start(Some(worker), true));
        drain.clear_retry_hold(live.id);
        assert!(!drain.holds_job_start(Some(worker), true));
        // Settled (no live row) and cancelled deploys release it as well.
        drain.hold_jobs_until_settled(&next, now);
        assert!(drain.holds_job_start(Some(worker), true));
        drain.sync(None, true, now);
        assert!(drain.jobs_held_at(now).is_none(), "the job hold ended");
        drain.clear_retry_hold(next.id);
        assert!(!drain.holds_job_start(Some(worker), true));
        drain.hold_jobs_until_settled(&next, now);
        drain.release(Uuid::new_v4());
        assert!(
            drain.holds_job_start(Some(worker), true),
            "another deploy's id"
        );
        drain.release(next.id);
        assert!(!drain.holds_job_start(Some(worker), true));
        assert!(!drain.status().draining);
    }

    /// #2278/#2551: a staged `interrupt_workers` deploy's job hold is continuous
    /// from its staged hold into the retry window at ONE boundary, the deadline:
    /// no instant between them admits a job, whether or not the runner has
    /// settled the timeout, and the window ends exactly `DEPLOY_JOB_RETRY_HOLD_SECS`
    /// later. A new staged deploy takes it over.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn the_job_hold_is_continuous_from_the_staged_hold_into_the_retry_window() {
        let drain = DeployDrain::new();
        let worker = Uuid::new_v4();
        let now = Utc::now();
        let ms = chrono::Duration::milliseconds(1);
        assert!(!drain.holds_job_start(Some(worker), true));

        let staged = row(Uuid::new_v4(), now + chrono::Duration::minutes(15));
        drain.hold_jobs_until_settled(&staged, now);
        let until = retry_hold_until(staged.deadline_at);
        assert_eq!(
            until - staged.deadline_at,
            chrono::Duration::seconds(300),
            "the window is DEPLOY_JOB_RETRY_HOLD_SECS"
        );
        // The deadline itself is inside the hold, with no gap on either side.
        for at in [
            now,
            staged.deadline_at - ms,
            staged.deadline_at,
            staged.deadline_at + ms,
            until - ms,
        ] {
            for has_parent in [true, false] {
                assert!(
                    drain.holds_job_start_at(Some(worker), has_parent, at),
                    "{at}"
                );
                assert!(drain.holds_job_start_at(None, has_parent, at), "{at}");
            }
        }
        assert!(!drain.holds_job_start_at(Some(worker), true, until));
        // The reported release time is the deadline while staged, the window's
        // end only once the deadline has passed.
        assert_eq!(drain.job_release_by(), Some(staged.deadline_at));
        drain.hold_jobs_until_settled(&staged, staged.deadline_at + ms);
        assert_eq!(drain.retry_hold_owner(), Some(staged.id));

        // A retry staged inside the window replaces it with its own hold.
        let retry = row(Uuid::new_v4(), now + chrono::Duration::minutes(30));
        drain.hold_jobs_until_settled(&retry, now);
        assert_eq!(drain.retry_hold_owner(), Some(retry.id));
        assert_eq!(drain.job_release_by(), Some(retry.deadline_at));
        // A settle without a timeout drops the window.
        drain.clear_retry_hold(retry.id);
        drain.sync(None, true, now);
        assert!(!drain.holds_job_start(Some(worker), true));
    }

    /// #2553: a cancel releases the retry hold of its own deploy only. A replayed
    /// cancel of an older deploy A must not drop deploy B's window.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_release_clears_the_retry_hold_of_its_own_deploy_only() {
        let drain = DeployDrain::new();
        let worker = Uuid::new_v4();
        let now = Utc::now();
        let older = row(Uuid::new_v4(), now + chrono::Duration::minutes(5));
        let current = row(Uuid::new_v4(), now + chrono::Duration::minutes(15));
        drain.hold_jobs_until_settled(&older, now);
        drain.hold_jobs_until_settled(&current, now);
        drain.sync(None, true, now);
        drain.release(older.id);
        drain.release(Uuid::new_v4());
        assert_eq!(drain.retry_hold_owner(), Some(current.id));
        assert!(
            drain.holds_job_start_at(Some(worker), true, current.deadline_at),
            "another deploy's cancel left the window alone"
        );
        drain.release(current.id);
        assert!(drain.retry_hold_owner().is_none());
        assert!(!drain.holds_job_start_at(Some(worker), true, current.deadline_at));
    }
}
