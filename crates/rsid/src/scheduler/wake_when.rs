//! Daemon-evaluated wait predicates (#1006): the scheduler checks a predicate
//! wake's condition itself and resumes the owner once, when it is true, the
//! predicate can never become true, or its timeout passes. No model turn is
//! spent polling.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rsi_common::agent_jobs::AgentJobV1;
use rsi_common::types::ScheduledJob;
use rsi_common::wake_predicate::{
    REASON_JOB_MISSING, REASON_REPO_UNAVAILABLE, WAKE_WHEN_MESSAGE_BYTES, WAKE_WHEN_REFUSAL_BYTES,
    WakeWhenState,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::store::Store;

/// Seconds between the scheduler's fast-lane passes over due predicate wakes.
pub(super) const FAST_LANE_SECS: u64 = 5;

/// What the scheduler does with a due predicate wake.
pub(super) enum Gate {
    /// Not a predicate wake: continue the ordinary Resume path.
    NotPredicate,
    /// Still pending; the row was deferred and nothing is delivered.
    Pending,
    /// Deliver this message as the (only) resume wake.
    Deliver(String),
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

/// The wake message: the caller's own text, then a bounded typed report.
pub(crate) fn compose_message(
    original: &str,
    state: &WakeWhenState,
    verdict: &Verdict,
    snapshots: &[JobSnapshot],
    sha: Option<&str>,
) -> String {
    let (status, timed_out, reason) = match verdict {
        Verdict::Satisfied => ("satisfied", false, None),
        Verdict::TimedOut => ("timed_out", true, None),
        Verdict::Unsatisfiable(reason) => ("unsatisfiable", false, Some(*reason)),
        Verdict::ManualPending | Verdict::Pending => ("pending_manual_trigger", false, None),
    };
    let subject = if state.predicate.sha_on_rolling.is_some() {
        format!("sha_on_rolling {}", sha.unwrap_or("-"))
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
    format!("{original}\n\n{body}")
}

fn snapshots(store: &Store, ids: &[Uuid]) -> Vec<JobSnapshot> {
    ids.iter()
        .map(|id| {
            let job = store.get_agent_job(*id).ok().flatten().map(|row| row.job);
            (*id, job)
        })
        .collect()
}

/// True when `sha` is an ancestor of `origin/rolling` in `repo_dir`. A missing
/// repository is `Err`; a missing commit or ref is simply "not yet".
fn sha_on_rolling(repo_dir: &str, sha: &str) -> Result<bool, ()> {
    if !std::path::Path::new(repo_dir).is_dir() {
        return Err(());
    }
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

/// Evaluate one predicate wake. `manual` (an operator trigger) delivers a
/// current-state report instead of deferring.
pub(super) async fn gate(store: &Arc<Mutex<Store>>, job: &ScheduledJob, manual: bool) -> Gate {
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
    let now = Utc::now();
    let (verdict, snaps) = if let Some(ids) = &state.predicate.jobs_terminal {
        let snaps = snapshots(&*store.lock().await, ids);
        (decide_jobs(&snaps, &state, now, manual), snaps)
    } else if let Some(sha) = state.predicate.sha_on_rolling.clone() {
        let repo = state.repo_dir.clone().unwrap_or_default();
        let probe = tokio::task::spawn_blocking(move || sha_on_rolling(&repo, &sha)).await;
        let verdict = match probe {
            Ok(Ok(true)) => Verdict::Satisfied,
            Ok(Err(())) => Verdict::Unsatisfiable(REASON_REPO_UNAVAILABLE),
            _ => decide_pending(&state, now, manual),
        };
        (verdict, Vec::new())
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
        verdict => Gate::Deliver(compose_message(
            &job.message,
            &state,
            &verdict,
            &snaps,
            state.predicate.sha_on_rolling.as_deref(),
        )),
    }
}
