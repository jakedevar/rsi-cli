//! Daemon-evaluated wait predicates (#1006): the scheduler checks a predicate
//! wake's condition itself and resumes the owner once, when it is true, the
//! predicate can never become true, or its timeout passes. No model turn is
//! spent polling.

use std::sync::Arc;

use crate::issue_tracker::poller::SessionLauncher;
use chrono::{DateTime, Utc};
use rsi_common::agent_jobs::AgentJobV1;
use rsi_common::operator_todos::{OperatorTodoStatusV1, OperatorTodoV1};
use rsi_common::topology_agent::AgentTopologyExecutionSummaryV1;
use rsi_common::types::{IssueStatus, ScheduledJob};
use rsi_common::wake_predicate::{
    QUIET_SETTLE_MAX_HOLD_SECS, REASON_EXECUTION_MISSING, REASON_ISSUE_CANCELLED,
    REASON_ISSUE_MISSING, REASON_JOB_MISSING, REASON_REPO_UNAVAILABLE,
    REASON_SETTLEMENT_RUN_MISSING, REASON_TODO_MISSING, WAKE_WHEN_MESSAGE_BYTES,
    WAKE_WHEN_REFUSAL_BYTES, WakeWhenState,
};
use uuid::Uuid;

use crate::store::Store;
use crate::store::scheduled_jobs::{SettleWakeDedupe, WakeWhenArmed};

/// Seconds between the scheduler's fast-lane passes over due predicate wakes.
pub(super) const FAST_LANE_SECS: u64 = 5;

/// Arm a predicate wake: the store checks what it can see (job ownership, an
/// Issue in the project, an operator to-do in scope), and a
/// `topology_execution_settled` execution is authorized here against the
/// topology authority rules (#2261) before the row is inserted. A manual arm
/// of a settle wake replaces the owner's other enabled settle wake for the same
/// execution, whatever its name (hub #2986).
pub(crate) fn arm_authorized(
    store: &Store,
    job: &ScheduledJob,
    state: &WakeWhenState,
    replace_name: bool,
) -> crate::error::Result<Vec<Uuid>> {
    Ok(
        arm_authorized_deduped(store, job, state, replace_name, SettleWakeDedupe::Replace)?
            .replaced,
    )
}

/// [`arm_authorized`] with the settle-wake rule chosen by the caller: the
/// daemon's default arm for `AgentTopologyExecute` passes
/// [`SettleWakeDedupe::SkipIfCovered`] and arms nothing when the owner already
/// has a settle wake for the execution.
pub(crate) fn arm_authorized_deduped(
    store: &Store,
    job: &ScheduledJob,
    state: &WakeWhenState,
    replace_name: bool,
    dedupe: SettleWakeDedupe,
) -> crate::error::Result<WakeWhenArmed> {
    if state.predicate.health_anomaly.is_some() {
        // #2747: a manager's predicate; the daemon, not the caller, decides
        // what it watches.
        let owner = job.wake_session_id.ok_or_else(|| {
            crate::error::DaemonError::InvalidParam(
                "mode 'when' requires a known origin session id".into(),
            )
        })?;
        let scoped = scope_health_predicate(store, owner, state)?;
        return store.insert_wake_when_settle_deduped(job, &scoped, replace_name, dedupe);
    }
    if let Some(execution_id) = state.predicate.topology_execution_settled {
        let owner = job.wake_session_id.ok_or_else(|| {
            crate::error::DaemonError::InvalidParam(
                "mode 'when' requires a known origin session id".into(),
            )
        })?;
        crate::topology::agent::authorize_execution_wake(store, owner, execution_id)?;
    }
    if let Some(run_id) = state.predicate.settlement_run_settled {
        let owner = job.wake_session_id.ok_or_else(|| {
            crate::error::DaemonError::InvalidParam(
                "mode 'when' requires a known origin session id".into(),
            )
        })?;
        authorize_settlement_run_wake(store, owner, run_id)?;
    }
    store.insert_wake_when_settle_deduped(job, state, replace_name, dedupe)
}

/// #2789: a `settlement_run_settled` wake is armed only by a caller with the
/// cohort's authority over the run's repository. An unknown run and a run the
/// caller may not read are the same refusal, so a run id proves nothing.
fn authorize_settlement_run_wake(
    store: &Store,
    owner: Uuid,
    run_id: Uuid,
) -> crate::error::Result<()> {
    let not_found = || {
        crate::error::DaemonError::InvalidParam(
            rsi_common::wake_predicate::WAKE_WHEN_SETTLEMENT_RUN_NOT_FOUND.into(),
        )
    };
    let run = store
        .get_source_worktree_settlement_run(run_id)?
        .ok_or_else(not_found)?;
    store
        .authorize_agent_source_worktree_cohort(owner, &run.repository_identity)
        .map_err(|_| not_found())
}

/// #2747: authorize a `health_anomaly` wake and pin its scope. A portfolio
/// seat watches the projects its grant covers (#2788); a project manager
/// watches its own project; an area seat watches its live grant. Any other
/// caller is refused. A client-supplied
/// `project` or `projects` is overwritten.
fn scope_health_predicate(
    store: &Store,
    owner: Uuid,
    state: &WakeWhenState,
) -> crate::error::Result<WakeWhenState> {
    let refused = || {
        crate::error::DaemonError::InvalidParam(
            rsi_common::wake_predicate::WAKE_WHEN_HEALTH_MANAGER_ONLY.into(),
        )
    };
    let projection = store
        .agent_authority_projection(owner)
        .map_err(|_| refused())?;
    let (project, projects) = if projection.is_global_manager {
        // #2788: a portfolio seat watches the projects its grant covers, which
        // for a child node is a strict subset of the fleet.
        let mut covered = store
            .portfolio_seat_grant(owner)?
            .map(|grant| grant.project_ids)
            .ok_or_else(refused)?;
        covered.sort();
        covered.dedup();
        (None, covered)
    } else if projection.is_manager {
        (
            Some(
                store
                    .get_session(owner)?
                    .and_then(|session| session.project_id)
                    .ok_or_else(refused)?,
            ),
            Vec::new(),
        )
    } else {
        return Err(refused());
    };
    let mut scoped = state.clone();
    if let Some(spec) = scoped.predicate.health_anomaly.as_mut() {
        spec.project = project;
        spec.projects = projects;
    }
    Ok(scoped)
}

/// What the scheduler does with a due predicate wake.
pub(super) enum Gate {
    /// Not a predicate wake: continue the ordinary Resume path.
    NotPredicate,
    /// Still pending; the row was deferred and nothing is delivered.
    Pending,
    /// Deliver this message as the (only) resume wake, with its logging outcome.
    Deliver(
        String,
        &'static str,
        Vec<rsi_common::health_anomaly::HealthCondition>,
    ),
    /// #217: deliver this message and keep the wake armed. A `fire: "each"`
    /// rolling wake found commits newly on rolling while others are still
    /// pending; once the delivery succeeds the scheduler records `delivered`
    /// (the whole reported set) and re-arms the row at `next_fire_at`.
    DeliverAndRearm {
        message: String,
        delivered: Vec<String>,
        next_fire_at: DateTime<Utc>,
    },
    /// #1935: the owner already wrote its `PIPELINE HANDOFF` after arming this
    /// wake; the settled wake is retired without opening a new turn.
    Retire,
}

/// How a predicate wake settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Satisfied,
    TimedOut,
    /// The predicate can never become true; the typed reason.
    Unsatisfiable(&'static str),
    /// An operator forced the trigger while the predicate was pending.
    ManualPending,
    /// #217: a `fire: "each"` rolling wake has commits newly on rolling and
    /// others still pending: this firing is delivered and the wake re-armed.
    Landed,
    Pending,
}

/// One listed job as the wake message reports it; `None` = no such job.
type JobSnapshot = (Uuid, Option<AgentJobV1>);

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Decide a `jobs_terminal` predicate from the job snapshots.
pub(crate) fn decide_jobs(
    snapshots: &[JobSnapshot],
    state: &WakeWhenState,
    now: DateTime<Utc>,
    manual: bool,
) -> Verdict {
    if snapshots.iter().any(|(_, job)| job.is_none()) {
        return Verdict::Unsatisfiable(REASON_JOB_MISSING);
    }
    if snapshots
        .iter()
        .all(|(_, job)| job.as_ref().is_some_and(|job| job.state.is_terminal()))
    {
        return Verdict::Satisfied;
    }
    decide_pending(state, now, manual)
}

fn decide_pending(state: &WakeWhenState, now: DateTime<Utc>, manual: bool) -> Verdict {
    if state.deadline.is_some_and(|deadline| now >= deadline) {
        Verdict::TimedOut
    } else if manual {
        Verdict::ManualPending
    } else {
        Verdict::Pending
    }
}

fn job_line(id: Uuid, job: Option<&AgentJobV1>) -> String {
    let Some(job) = job else {
        return format!("- job {id}: state=missing");
    };
    let mut line = format!(
        "- job {} name={} state={}",
        job.id,
        job.name.as_deref().unwrap_or("-"),
        job.state.as_str()
    );
    if let Some(code) = job
        .exit_code
        .or(job.result.as_ref().and_then(|r| r.exit_code))
    {
        line.push_str(&format!(" exit_code={code}"));
    }
    if let Some(refusal) = job.result.as_ref().and_then(|r| r.refusal.as_deref()) {
        line.push_str(&format!(
            " refusal={}",
            truncate(refusal, WAKE_WHEN_REFUSAL_BYTES)
        ));
    }
    line
}

/// A job a staged deploy stopped and requeued under its own id (#2278), as
/// `(job id, deploy id)`.
pub(crate) type RequeuedJob = (Uuid, Uuid);

/// The wake message: the caller's own text, then a bounded typed report.
/// `requeued` names the listed jobs a deploy stopped and requeued (#2204): the
/// state each line shows is the rerun's, so the resumed turn must not read an
/// interruption into it or submit the work again.
pub(crate) fn compose_message(
    original: &str,
    state: &WakeWhenState,
    verdict: &Verdict,
    snapshots: &[JobSnapshot],
    requeued: &[RequeuedJob],
    sha: Option<&str>,
) -> String {
    let (status, timed_out, reason) = match verdict {
        Verdict::Satisfied => ("satisfied", false, None),
        Verdict::Landed => ("landed", false, None),
        Verdict::TimedOut => ("timed_out", true, None),
        Verdict::Unsatisfiable(reason) => ("unsatisfiable", false, Some(*reason)),
        Verdict::ManualPending | Verdict::Pending => ("pending_manual_trigger", false, None),
    };
    let subject = if state.predicate.health_anomaly.is_some() {
        "health_anomaly".to_owned()
    } else if state.predicate.sha_on_rolling.is_some() {
        format!("sha_on_rolling {}", sha.unwrap_or("-"))
    } else if let Some(shas) = &state.predicate.shas_on_rolling {
        if state.predicate.fires_each() {
            format!("shas_on_rolling ({} commits, fire=each)", shas.len())
        } else {
            format!("shas_on_rolling ({} commits)", shas.len())
        }
    } else if let Some(number) = state.predicate.issue_closed {
        format!("issue_closed #{number}")
    } else if let Some(id) = state.predicate.todo_closed {
        format!("todo_closed {id}")
    } else if let Some(id) = state.predicate.topology_execution_settled {
        format!("topology_execution_settled {id}")
    } else if let Some(id) = state.predicate.settlement_run_settled {
        format!("settlement_run_settled {id}")
    } else {
        format!("jobs_terminal ({} jobs)", snapshots.len())
    };
    let mut body = format!("[wake_when] {subject}: {status}\ntimed_out: {timed_out}");
    if let Some(reason) = reason {
        body.push_str(&format!("\nreason: {reason}"));
    }
    let mut omitted = 0usize;
    for (id, job) in snapshots {
        let line = job_line(*id, job.as_ref());
        if body.len() + line.len() + 1 > WAKE_WHEN_MESSAGE_BYTES {
            omitted += 1;
        } else {
            body.push('\n');
            body.push_str(&line);
        }
    }
    if omitted > 0 {
        body.push_str(&format!(
            "\n({omitted} more jobs omitted; read them with AgentListJobs)"
        ));
    }
    for (job, deploy) in requeued {
        // Bounded like the job lines: the note is dropped, not the report.
        let note = format!(
            "\nnote: job {job} was stopped by deploy {deploy} and requeued by it under the \
             same id; the state above is the rerun's. Do not resubmit it."
        );
        if body.len() + note.len() <= WAKE_WHEN_MESSAGE_BYTES {
            body.push_str(&note);
        }
    }
    format!("{original}\n\n{body}")
}

/// The listed jobs a deploy requeued (#2204). An unreadable record reports
/// nothing: the note is advice, never a reason to hold the wake.
fn requeued_jobs(store: &Store, ids: &[Uuid]) -> Vec<RequeuedJob> {
    ids.iter()
        .filter_map(|id| {
            let deploy = store.agent_job_requeued_by_deploy(*id).ok().flatten()?;
            Some((*id, deploy))
        })
        .collect()
}

fn snapshots(store: &Store, ids: &[Uuid]) -> Vec<JobSnapshot> {
    ids.iter()
        .map(|id| {
            let job = store.get_agent_job(*id).ok().flatten().map(|row| row.job);
            (*id, job)
        })
        .collect()
}

/// Upper bound on one `git fetch` of `origin rolling` made for a pending
/// rolling commit probe (#1463).
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

fn is_ancestor_of_origin_rolling(repo_dir: &str, sha: &str) -> Result<bool, ()> {
    let status = std::process::Command::new("git")
        .args([
            "-C",
            repo_dir,
            "merge-base",
            "--is-ancestor",
            sha,
            "origin/rolling",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| ())?;
    Ok(status.success())
}

/// Refresh the local `origin/rolling` tracking ref, bounded by `timeout`.
/// Rolling can advance (the lander publishes straight to the remote) without
/// anything fetching into this repository, so the local ref alone goes stale
/// (#1463). Best effort: a failed or timed-out fetch leaves the old ref.
fn fetch_origin_rolling(repo_dir: &str, timeout: std::time::Duration) {
    let Ok(mut child) = std::process::Command::new("git")
        .args([
            "-C",
            repo_dir,
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "origin",
            "+refs/heads/rolling:refs/remotes/origin/rolling",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// Check each commit against the local tracking ref, fetching once per batch
/// if any are pending, then rechecking every commit. A missing
/// repository is `Err`; a missing commit or ref is simply "not yet".
fn shas_on_rolling(repo_dir: &str, shas: &[String]) -> Result<Vec<bool>, ()> {
    if !std::path::Path::new(repo_dir).is_dir() {
        return Err(());
    }
    let check = || {
        shas.iter()
            .map(|sha| is_ancestor_of_origin_rolling(repo_dir, sha))
            .collect::<Result<Vec<_>, ()>>()
    };
    let states = check()?;
    if states.iter().all(|on_rolling| *on_rolling) {
        return Ok(states);
    }
    fetch_origin_rolling(repo_dir, FETCH_TIMEOUT);
    check()
}

/// Decide a rolling commit predicate (`sha_on_rolling`, `shas_on_rolling`)
/// from the probe (`None` = git could not answer), and render its report: one
/// `sha=<id> state=on_rolling|pending|unknown` line per listed commit.
///
/// #217: with `fire: "each"`, commits on rolling that no earlier firing
/// reported (`state.delivered_shas`) make a pending predicate [`Verdict::Landed`]:
/// the report then opens with `landed_since_last_firing:` and marks those
/// lines `new=true`. The third value is the delivered set after this firing
/// (earlier ones first); it is empty for the default mode.
pub(crate) fn decide_rolling(
    shas: &[String],
    states: Option<&[bool]>,
    state: &WakeWhenState,
    now: DateTime<Utc>,
    manual: bool,
) -> (Verdict, String, Vec<String>) {
    let on_rolling = |index: usize| states.is_some_and(|states| states[index]);
    let each = state.predicate.fires_each();
    let newly: Vec<&String> = if each {
        shas.iter()
            .enumerate()
            .filter(|(index, sha)| on_rolling(*index) && !state.delivered_shas.contains(sha))
            .map(|(_, sha)| sha)
            .collect()
    } else {
        Vec::new()
    };
    let mut lines = Vec::with_capacity(shas.len() + 2);
    if each {
        let landed = if newly.is_empty() {
            "none".to_owned()
        } else {
            newly
                .iter()
                .map(|sha| sha.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        lines.push(format!("landed_since_last_firing: {landed}"));
    }
    for (index, sha) in shas.iter().enumerate() {
        let status = match states {
            Some(states) if states[index] => "on_rolling",
            Some(_) => "pending",
            None => "unknown",
        };
        let mut line = format!("sha={sha} state={status}");
        if newly.contains(&sha) {
            line.push_str(" new=true");
        }
        lines.push(line);
    }
    let verdict = if states.is_some_and(|states| states.iter().all(|on| *on)) {
        Verdict::Satisfied
    } else {
        match decide_pending(state, now, manual) {
            Verdict::Pending if !newly.is_empty() => Verdict::Landed,
            verdict => verdict,
        }
    };
    if verdict == Verdict::Landed {
        let pending = shas.len() - states.map_or(0, |s| s.iter().filter(|on| **on).count());
        lines.push(format!(
            "remaining: {pending} pending; this wake stays armed and fires again as each lands"
        ));
    }
    let mut delivered = state.delivered_shas.clone();
    delivered.extend(newly.into_iter().cloned());
    (
        verdict,
        lines.join("\n"),
        if each { delivered } else { Vec::new() },
    )
}

/// Decide an `issue_closed` predicate from the Issue's current status
/// (`None` = no such Issue).
pub(crate) fn decide_issue(
    status: Option<IssueStatus>,
    state: &WakeWhenState,
    now: DateTime<Utc>,
    manual: bool,
) -> Verdict {
    match status {
        Some(IssueStatus::Closed) => Verdict::Satisfied,
        Some(IssueStatus::Cancelled) => Verdict::Unsatisfiable(REASON_ISSUE_CANCELLED),
        None => Verdict::Unsatisfiable(REASON_ISSUE_MISSING),
        Some(IssueStatus::Open | IssueStatus::InProgress) => decide_pending(state, now, manual),
    }
}

/// Decide a `todo_closed` predicate from the item's current status (`None` =
/// no such item). `done` and `dismissed` both close it.
pub(crate) fn decide_todo(
    status: Option<OperatorTodoStatusV1>,
    state: &WakeWhenState,
    now: DateTime<Utc>,
    manual: bool,
) -> Verdict {
    match status {
        Some(OperatorTodoStatusV1::Done | OperatorTodoStatusV1::Dismissed) => Verdict::Satisfied,
        None => Verdict::Unsatisfiable(REASON_TODO_MISSING),
        Some(OperatorTodoStatusV1::Open) => decide_pending(state, now, manual),
    }
}

/// Decide a `topology_execution_settled` predicate from the execution's
/// current status (`None` = no such execution). `succeeded`, `failed`,
/// `cancelled` and `blocked` all settle it (#2261); a blocked execution that is
/// later resolved needs a new wake.
pub(crate) fn decide_execution(
    status: Option<&str>,
    state: &WakeWhenState,
    now: DateTime<Utc>,
    manual: bool,
) -> Verdict {
    match status {
        Some("succeeded" | "failed" | "cancelled" | "blocked") => Verdict::Satisfied,
        None => Verdict::Unsatisfiable(REASON_EXECUTION_MISSING),
        Some(_) => decide_pending(state, now, manual),
    }
}

/// Decide a `settlement_run_settled` predicate from the run row (`None` = no
/// such run). A run is finished once every root reached a terminal phase, which
/// is when the store stamps `finished_at`; `partial` with `finished_at` unset is
/// still applying.
pub(crate) fn decide_settlement_run(
    run: Option<&rsi_common::cohort_settlement::SourceWorktreeSettlementRunV1>,
    state: &WakeWhenState,
    now: DateTime<Utc>,
    manual: bool,
) -> Verdict {
    match run {
        Some(run) if run.finished_at.is_some() => Verdict::Satisfied,
        None => Verdict::Unsatisfiable(REASON_SETTLEMENT_RUN_MISSING),
        Some(_) => decide_pending(state, now, manual),
    }
}

/// Bound on the run's terminal error carried by a `settlement_run_settled` wake.
const SETTLEMENT_ERROR_BYTES: usize = 1_024;

/// The report line for the awaited run: state, per-phase counts and any
/// terminal error.
fn settlement_run_line(
    id: Uuid,
    run: Option<&rsi_common::cohort_settlement::SourceWorktreeSettlementRunV1>,
) -> String {
    let Some(run) = run else {
        return format!("- settlement run {id}: state=missing");
    };
    let counts = &run.counts;
    let mut line = format!(
        "- settlement_run_id={id} state={} eligible={} settled={} refused={} \
         recovery_required={} unattempted={}",
        run.state.as_str(),
        counts.eligible,
        counts.settled,
        counts.refused,
        counts.recovery_required,
        counts.unattempted
    );
    if let Some(error) = run.terminal_error.as_deref().filter(|e| !e.is_empty()) {
        line.push_str(&format!(
            " terminal_error={}",
            truncate(error, SETTLEMENT_ERROR_BYTES)
        ));
    }
    if let Some(finished) = &run.finished_at {
        line.push_str(&format!(" finished_at={finished}"));
    }
    line
}

/// Bound on the execution error carried by a `topology_execution_settled` wake.
const EXECUTION_ERROR_BYTES: usize = 1_024;

/// The report line for the awaited execution: status, error, the blocked
/// reason summary (`kind`, `node_id`, `attempt_id`) and finish time.
fn execution_line(id: Uuid, execution: Option<&AgentTopologyExecutionSummaryV1>) -> String {
    let Some(execution) = execution else {
        return format!("- execution {id}: status=missing");
    };
    let mut line = format!("- execution_id={id} status={}", execution.status);
    if let Some(error) = execution.error.as_deref().filter(|e| !e.is_empty()) {
        line.push_str(&format!(
            " error={}",
            truncate(error, EXECUTION_ERROR_BYTES)
        ));
    }
    if let Some(reason) = &execution.blocked_reason {
        let field = |name: &str| {
            reason
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
        };
        line.push_str(&format!(
            " blocked_reason=kind:{} node_id:{} attempt_id:{}",
            field("kind"),
            field("node_id"),
            field("attempt_id")
        ));
    }
    if let Some(finished) = &execution.finished_at {
        line.push_str(&format!(" finished_at={finished}"));
    }
    line
}

/// The report line for the awaited item: its status and closing evidence.
fn todo_line(id: Uuid, todo: Option<&OperatorTodoV1>) -> String {
    let Some(todo) = todo else {
        return format!("- todo {id}: status=missing");
    };
    let mut line = format!(
        "- todo {id} key={} status={}",
        todo.todo_key,
        todo.status.as_str()
    );
    if let Some(by) = &todo.closed_by {
        line.push_str(&format!(" closed_by={by}"));
    }
    if let Some(evidence) = todo.evidence.as_deref().filter(|e| !e.is_empty()) {
        line.push_str(&format!(
            " evidence={}",
            truncate(evidence, TODO_EVIDENCE_BYTES)
        ));
    }
    line
}

/// Bound on the closing evidence carried by a `todo_closed` wake message.
const TODO_EVIDENCE_BYTES: usize = 2_048;

/// #1935: the wake's owner has already handed off since arming it.
///
/// #2054: only when the predicate had already settled (`settled_at`) before
/// that handoff. A handoff written while the predicate was still pending is a
/// provisional marker on a parked turn; the worker has not seen the result, so
/// the wake delivers.
async fn wake_superseded(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    job: &ScheduledJob,
    settled_at: Option<DateTime<Utc>>,
) -> bool {
    let Some(owner) = job.wake_session_id else {
        return false;
    };
    match store
        .lock()
        .await
        .wake_superseded_by_handoff(owner, job.created_at, settled_at)
    {
        Ok(superseded) => superseded,
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "wake supersession unreadable; delivering");
            false
        }
    }
}

/// #2054: when a satisfied predicate became true, if the daemon recorded it.
/// `jobs_terminal` settles when its last job finished; `todo_closed` when the
/// item closed. Rolling commit predicates and `issue_closed` record no such time, so
/// they are `None` and a wake on them is never treated as superseded: a late
/// short notice is cheaper than a lost result.
fn settled_at(
    state: &WakeWhenState,
    snaps: &[JobSnapshot],
    todo_closed_at: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    if state.predicate.jobs_terminal.is_some() {
        return snaps
            .iter()
            .map(|(_, job)| {
                let finished = job.as_ref()?.finished_at.as_deref()?;
                DateTime::parse_from_rfc3339(finished)
                    .ok()
                    .map(|at| at.with_timezone(&Utc))
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max();
    }
    todo_closed_at
}

/// #3387: a quiet-capable settle wake (the daemon's default arm; see
/// [`rsi_common::wake_predicate::QuietSettle`]) that observed a clean success
/// opens no owner turn. The success is recorded on the row and the row's next
/// check moves to the batch deadline: the owner's next wake carries the digest
/// ([`append_quiet_digest`]), and once the oldest held line is
/// `QUIET_SETTLE_MAX_HOLD_SECS` old the wake is delivered alone as one batch.
/// `None` delivers the wake as usual: the owner armed it itself, the record is
/// unreadable, or the batch deadline passed.
async fn quiet_hold(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    job: &ScheduledJob,
    line: String,
    now: DateTime<Utc>,
) -> Option<Gate> {
    let guard = store.lock().await;
    let record = match guard.quiet_settle_state(job.id) {
        Ok(Some(record)) => record,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "quiet settle record unreadable; delivering");
            return None;
        }
    };
    let held = match record.held {
        Some(held) => held,
        None => {
            let held = rsi_common::wake_predicate::QuietHeld { at: now, line };
            let due = now + chrono::Duration::seconds(QUIET_SETTLE_MAX_HOLD_SECS);
            if let Err(error) = guard.hold_quiet_settle(job.id, &held, due) {
                tracing::warn!(job_id = %job.id, %error, "clean success not held; delivering");
                return None;
            }
            held
        }
    };
    let due = held.at + chrono::Duration::seconds(QUIET_SETTLE_MAX_HOLD_SECS);
    if now >= due {
        return None;
    }
    if let Err(error) = guard.defer_wake_when(job.id, due) {
        tracing::warn!(job_id = %job.id, %error, "failed to defer held clean success");
    }
    Some(Gate::Pending)
}

/// Most held successes listed in one digest; the rest are counted.
const QUIET_DIGEST_LINES: usize = 20;

/// #3387: the owner's held clean successes as digest lines to append to a wake
/// delivered to it now, with the rows that carry them (retired by the caller
/// once the owner took the turn). Empty when nothing is held or the read fails.
pub(super) async fn quiet_digest(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    job: &ScheduledJob,
) -> (Vec<Uuid>, String) {
    let Some(owner) = job.wake_session_id else {
        return (Vec::new(), String::new());
    };
    let held = match store.lock().await.held_quiet_settles(owner, job.id) {
        Ok(held) => held,
        Err(error) => {
            tracing::warn!(job_id = %job.id, %error, "held clean successes unreadable; none appended");
            return (Vec::new(), String::new());
        }
    };
    if held.is_empty() {
        return (Vec::new(), String::new());
    }
    let mut text = format!(
        "\nClean topology successes since your last wake ({}; no action needed):",
        held.len()
    );
    for (_, entry) in held.iter().take(QUIET_DIGEST_LINES) {
        text.push_str(&format!(
            "\n- {} (settled {})",
            entry.line,
            entry.at.to_rfc3339()
        ));
    }
    if held.len() > QUIET_DIGEST_LINES {
        text.push_str(&format!("\n(+{} more)", held.len() - QUIET_DIGEST_LINES));
    }
    (held.into_iter().map(|(id, _)| id).collect(), text)
}

/// Evaluate one predicate wake. `manual` (an operator trigger) delivers a
/// current-state report instead of deferring.
pub(super) async fn gate(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &ScheduledJob,
    manual: bool,
) -> Gate {
    gate_at(store, launcher, job, manual, Utc::now()).await
}

pub(super) async fn gate_at(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &ScheduledJob,
    manual: bool,
    now: DateTime<Utc>,
) -> Gate {
    let state = {
        let guard = store.lock().await;
        match guard.wake_when_state(job.id) {
            Ok(Some(state)) => state,
            Ok(None) => return Gate::NotPredicate,
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "wake predicate unreadable; wake stays armed");
                return Gate::Pending;
            }
        }
    };
    let mut todo_report: Option<String> = None;
    let mut health_conditions = Vec::new();
    let mut todo_closed_at: Option<DateTime<Utc>> = None;
    // #3387: set when the awaited execution succeeded and asks nothing of its owner.
    let mut quiet_line: Option<String> = None;
    let mut requeued: Vec<RequeuedJob> = Vec::new();
    let mut each_delivered: Vec<String> = Vec::new();
    let (verdict, snaps) = if let Some(ids) = &state.predicate.jobs_terminal {
        let snaps = {
            let guard = store.lock().await;
            requeued = requeued_jobs(&guard, ids);
            snapshots(&guard, ids)
        };
        (decide_jobs(&snaps, &state, now, manual), snaps)
    } else if let Some(shas) = state
        .predicate
        .shas_on_rolling
        .clone()
        .or_else(|| state.predicate.sha_on_rolling.clone().map(|sha| vec![sha]))
    {
        let repo = state.repo_dir.clone().unwrap_or_default();
        let probe_shas = shas.clone();
        let probe = tokio::task::spawn_blocking(move || shas_on_rolling(&repo, &probe_shas)).await;
        let states = match &probe {
            Ok(Ok(states)) => Some(states.as_slice()),
            _ => None,
        };
        let (verdict, report, delivered) = decide_rolling(&shas, states, &state, now, manual);
        todo_report = Some(report);
        each_delivered = delivered;
        let verdict = match probe {
            Ok(Err(())) => Verdict::Unsatisfiable(REASON_REPO_UNAVAILABLE),
            _ => verdict,
        };
        (verdict, Vec::new())
    } else if let Some(number) = state.predicate.issue_closed {
        let status = match job.project_id {
            Some(project) => store.lock().await.issue_status_by_number(project, number),
            None => Ok(None),
        };
        match status {
            Ok(status) => (decide_issue(status, &state, now, manual), Vec::new()),
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "issue predicate unreadable; wake stays armed");
                (decide_pending(&state, now, manual), Vec::new())
            }
        }
    } else if let Some(id) = state.predicate.todo_closed {
        let todo = store.lock().await.operator_todo_by_id(id);
        match todo {
            Ok(todo) => {
                let verdict = decide_todo(todo.as_ref().map(|t| t.status), &state, now, manual);
                todo_report = Some(todo_line(id, todo.as_ref()));
                todo_closed_at = todo.as_ref().and_then(|t| t.closed_at);
                (verdict, Vec::new())
            }
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "todo predicate unreadable; wake stays armed");
                (decide_pending(&state, now, manual), Vec::new())
            }
        }
    } else if let Some(id) = state.predicate.topology_execution_settled {
        let execution = {
            let guard = store.lock().await;
            crate::topology::agent::execution_summary(&guard.conn, id)
        };
        match execution {
            Ok(execution) => {
                let verdict = decide_execution(
                    execution.as_ref().map(|e| e.status.as_str()),
                    &state,
                    now,
                    manual,
                );
                let mut report = execution_line(id, execution.as_ref());
                // #2559: a settled execution's wake carries what the manager
                // would otherwise read four calls to learn.
                if matches!(verdict, Verdict::Satisfied)
                    && execution
                        .as_ref()
                        .is_some_and(|e| crate::topology::outcome::is_settled(&e.status))
                {
                    let guard = store.lock().await;
                    match crate::topology::outcome::settled_outcome(&guard, id) {
                        Ok(Some(outcome)) => {
                            if execution.as_ref().is_some_and(|e| e.status == "succeeded")
                                && crate::topology::outcome::is_clean_success(&outcome)
                            {
                                quiet_line =
                                    Some(crate::topology::outcome::digest_line(id, &outcome));
                            }
                            report.push('\n');
                            report.push_str(&crate::topology::outcome::render(
                                &outcome,
                                crate::topology::outcome::OUTCOME_WAKE_BYTES,
                            ));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(job_id = %job.id, %error, "execution outcome unreadable; wake carries the status only");
                        }
                    }
                }
                todo_report = Some(report);
                todo_closed_at = execution
                    .as_ref()
                    .and_then(|e| e.finished_at.as_deref())
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc));
                (verdict, Vec::new())
            }
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "execution predicate unreadable; wake stays armed");
                (decide_pending(&state, now, manual), Vec::new())
            }
        }
    } else if let Some(id) = state.predicate.settlement_run_settled {
        let run = store.lock().await.get_source_worktree_settlement_run(id);
        match run {
            Ok(run) => {
                let verdict = decide_settlement_run(run.as_ref(), &state, now, manual);
                todo_report = Some(settlement_run_line(id, run.as_ref()));
                todo_closed_at = run
                    .as_ref()
                    .and_then(|r| r.finished_at.as_deref())
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc));
                (verdict, Vec::new())
            }
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "settlement run predicate unreadable; wake stays armed");
                (decide_pending(&state, now, manual), Vec::new())
            }
        }
    } else if let Some(spec) = state.predicate.health_anomaly.clone() {
        // #2747: no model turn; the daemon checks the facts itself and wakes
        // the owner only when one holds.
        // #2788: the scope is re-derived from the owner's grant on every
        // pass, so a narrowed or revoked grant stops reporting at once.
        let facts = {
            let guard = store.lock().await;
            crate::health_anomaly::Scope::live(&guard, job.wake_session_id, &spec).and_then(
                |scope| {
                    crate::health_anomaly::collect_store_facts(&guard, &scope, now).and_then(
                        |facts| {
                            let reported = job
                                .wake_session_id
                                .map(|owner| guard.provider_window_warning_subjects(owner, now))
                                .transpose()?
                                .unwrap_or_default();
                            Ok((scope, facts, reported))
                        },
                    )
                },
            )
        };
        match facts {
            Ok((scope, facts, reported)) => {
                let inputs = crate::health_anomaly::Inputs {
                    store: facts,
                    host: crate::health_anomaly::read_host(),
                    live: launcher.health_live_inputs(&scope).await,
                    helper_outages: crate::memory::helper_breaker::HelperEndpointBreaker::global()
                        .outages(std::time::Instant::now()),
                    daemon_stops: crate::daemon_lifecycle::journal_stops(),
                    supervisor_restarts: rsi_common::supervisor_restarts::SupervisorRestarts::read(
                        &rsi_common::supervisor_restarts::default_path(),
                    ),
                };
                let conditions = {
                    let mut trackers = crate::health_anomaly::sustain()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let mut window_trackers = crate::health_anomaly::provider_windows::trackers()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Session identity scopes episode notifications; a re-armed
                    // wake retains samples and acknowledgments from its owner.
                    window_trackers.retain(|_, tracker| tracker.retain_live(now));
                    let owner = job.wake_session_id.unwrap_or(job.id);
                    let tracker = trackers.entry(job.id).or_default();
                    tracker.windows = window_trackers.remove(&owner).unwrap_or_default();
                    for subject in reported {
                        tracker.windows.reported_subject(subject);
                    }
                    let mut found = crate::health_anomaly::hub_pull_outages()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .evaluate(
                            (owner, job.id),
                            &inputs,
                            &rsid_core::health_thresholds::current(),
                            &spec,
                            tracker,
                            now,
                        );
                    window_trackers.insert(owner, std::mem::take(&mut tracker.windows));
                    // #4020: the daemon-armed manager idle backstop (#3765) is
                    // re-armed by every turn end, so it reports an anomaly set
                    // once per seat, not once per re-arm.
                    if job
                        .name
                        .starts_with(crate::session::manager_idle::IDLE_BACKSTOP_NAME_PREFIX)
                        && let Some(seat) = job.wake_session_id
                    {
                        found = crate::health_anomaly::idle_backstop_latch()
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .filter(seat, job.id, found);
                    }
                    if !found.is_empty() {
                        trackers.remove(&job.id);
                    }
                    found
                };
                let verdict = if conditions.is_empty() {
                    decide_pending(&state, now, manual)
                } else {
                    Verdict::Satisfied
                };
                if !conditions.is_empty() {
                    health_conditions = conditions.clone();
                    let mut report = conditions
                        .iter()
                        .map(rsi_common::health_anomaly::HealthCondition::line)
                        .collect::<Vec<_>>()
                        .join("\n");
                    report.push_str(
                        "\nHandle each condition, then re-arm health_anomaly; add its \
                         key (the ignore_key= field, verbatim) to ignore to accept one that will persist.",
                    );
                    if conditions.iter().any(|condition| {
                        condition.kind == rsi_common::health_anomaly::KIND_HUB_NOT_PULLING
                    }) {
                        report.push_str(
                            " hub_not_pulling is latched for this outage: re-arm with your usual spec; \
                             do not add its key to ignore, a later outage after a successful pull fires again.",
                        );
                    }
                    todo_report = Some(report);
                }
                (verdict, Vec::new())
            }
            Err(error) => {
                tracing::error!(job_id = %job.id, %error, "health predicate unreadable; wake stays armed");
                (decide_pending(&state, now, manual), Vec::new())
            }
        }
    } else {
        // A stored predicate that names nothing can never become true.
        (Verdict::Unsatisfiable("predicate_empty"), Vec::new())
    };
    match verdict {
        Verdict::Pending => {
            let next = now + chrono::Duration::seconds(state.predicate.poll_seconds());
            // Never sleep past the deadline: the timeout must fire on time.
            let next = state.deadline.map_or(next, |deadline| next.min(deadline));
            if let Err(error) = store.lock().await.defer_wake_when(job.id, next) {
                tracing::error!(job_id = %job.id, %error, "failed to defer pending wake predicate");
            }
            Gate::Pending
        }
        Verdict::Satisfied
            if !manual
                && wake_superseded(store, job, settled_at(&state, &snaps, todo_closed_at))
                    .await =>
        {
            Gate::Retire
        }
        verdict => {
            if let (Verdict::Satisfied, false, Some(line)) = (&verdict, manual, quiet_line)
                && let Some(gate) = quiet_hold(store, job, line, now).await
            {
                return gate;
            }
            let mut message = compose_message(
                &job.message,
                &state,
                &verdict,
                &snaps,
                &requeued,
                state.predicate.sha_on_rolling.as_deref(),
            );
            if let Some(line) = todo_report {
                message.push('\n');
                message.push_str(&line);
            }
            if verdict == Verdict::Landed {
                let next = now + chrono::Duration::seconds(state.predicate.poll_seconds());
                Gate::DeliverAndRearm {
                    message,
                    delivered: each_delivered,
                    next_fire_at: state.deadline.map_or(next, |deadline| next.min(deadline)),
                }
            } else {
                let outcome = match verdict {
                    Verdict::Satisfied => "satisfied",
                    Verdict::TimedOut => "timed_out",
                    Verdict::Unsatisfiable(_) => "unsatisfiable",
                    Verdict::ManualPending => "manual_pending",
                    Verdict::Landed => "landed",
                    Verdict::Pending => "pending",
                };
                Gate::Deliver(message, outcome, health_conditions)
            }
        }
    }
}
