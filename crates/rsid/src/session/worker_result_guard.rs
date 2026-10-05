//! Issue #1098: a worker never silently ends without a `RESULT` line.
//!
//! A leaf worker with a parent (a manager or lead owns it) that ends a turn
//! without a final `RESULT ...` line while a process it spawned or a daemon job
//! it owns is still running gets exactly one automatic continuation: a one-shot
//! same-session Resume wake telling it to wait for the run in the foreground
//! and then report. A second miss settles the turn with
//! `stop_reason = "no_result"` (surfaced as the terminal reason) and never
//! continues again. Operator sessions, managers and Epic leads are untouched.

use std::sync::Arc;

use rsi_common::types::{Session, SessionStatus};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::store::Store;

/// Closed `stop_reason` recorded when the worker misses its RESULT twice.
pub(crate) const NO_RESULT_STOP_REASON: &str = "no_result";

// The store half (constants and `impl Store`) lives in `store::worker_no_result`
// so `store` has no edge into `session` (issue #1021 S4).
pub(crate) use crate::store::worker_no_result::{
    NO_RESULT_MARKER, NO_RESULT_RETIRED, NO_RESULT_WAKE_NAME_PREFIX, no_result_wake_id,
};

/// True for the refusal that retires a no-result continuation wake.
pub(crate) fn is_no_result_retired(error: &crate::error::DaemonError) -> bool {
    matches!(error, crate::error::DaemonError::InvalidParam(code) if code == NO_RESULT_RETIRED)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoResultVerdict {
    /// Not a covered worker, or the turn reported its RESULT, or nothing runs.
    NotApplicable,
    /// First miss with owned work still running: continue once.
    FirstMiss,
    /// Already continued once and still no RESULT: settle `no_result`.
    SecondMiss,
}

/// True when any line of `text` is a `RESULT` report line.
pub(crate) fn has_result_line(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, '*' | '#' | '>' | '`' | '-' | '_')
        });
        line == "RESULT"
            || line
                .strip_prefix("RESULT")
                .is_some_and(|rest| rest.starts_with([' ', ':', '\t']))
    })
}

/// Live (non-zombie) descendants of `pid`, by the `/proc` parent chain.
#[cfg(target_os = "linux")]
pub(crate) fn live_descendant_count(pid: u32) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut parent_of = std::collections::HashMap::<u32, u32>::new();
    let mut zombies = std::collections::HashSet::<u32>::new();
    for entry in entries.flatten() {
        let Some(child) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid ...`; comm may contain spaces and parens.
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if matches!(state, Some("Z" | "X" | "x")) {
            zombies.insert(child);
        }
        parent_of.insert(child, ppid);
    }
    let mut live = 0;
    for (&child, _) in &parent_of {
        if zombies.contains(&child) {
            continue;
        }
        let mut cursor = child;
        let mut hops = 0;
        while let Some(&parent) = parent_of.get(&cursor) {
            if parent == pid {
                live += 1;
                break;
            }
            if parent <= 1 || hops > 64 {
                break;
            }
            cursor = parent;
            hops += 1;
        }
    }
    live
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn live_descendant_count(_pid: u32) -> usize {
    0
}

#[cfg(test)]
fn test_provider_pids() -> &'static std::sync::Mutex<std::collections::HashMap<Uuid, u32>> {
    static PIDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Uuid, u32>>> =
        std::sync::OnceLock::new();
    PIDS.get_or_init(Default::default)
}

/// Test seam: stand in a real pid for a scripted provider process.
#[cfg(test)]
pub(crate) fn install_test_provider_pid(session_id: Uuid, pid: u32) {
    test_provider_pids()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id, pid);
}

/// Whether the worker's provider process still has live descendants. Called
/// at the result boundary, before terminal settlement reaps the cohort.
pub(crate) async fn owned_children_alive(
    active: &Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>,
    >,
    session_id: Uuid,
) -> bool {
    let pid = {
        let guard = active.read().await;
        let Some(tracked) = guard.get(&session_id) else {
            return false;
        };
        // Cheap gate: only leaf workers with a parent are ever covered.
        if tracked.session.parent_id.is_none()
            || !rsi_common::is_leaf_kind(tracked.session.session_kind)
        {
            return false;
        }
        let pid = tracked.process.as_ref().and_then(|p| p.os_pid());
        #[cfg(test)]
        let pid = pid.or_else(|| {
            test_provider_pids()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&session_id)
                .copied()
        });
        pid
    };
    let Some(pid) = pid else {
        return false;
    };
    tokio::task::spawn_blocking(move || live_descendant_count(pid) > 0)
        .await
        .unwrap_or(false)
}

/// Decide the verdict for a worker turn about to finalize `Completed`.
pub(crate) async fn worker_no_result_verdict(
    store: &Arc<Mutex<Store>>,
    session: &Session,
    assistant_output: &str,
    children_at_result: bool,
) -> NoResultVerdict {
    if session.status == SessionStatus::Archived || has_result_line(assistant_output) {
        return NoResultVerdict::NotApplicable;
    }
    let guard = store.lock().await;
    let covered = match guard.worker_no_result_covered(session) {
        Ok(covered) => covered,
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "no-result worker check failed");
            return NoResultVerdict::NotApplicable;
        }
    };
    if !covered {
        return NoResultVerdict::NotApplicable;
    }
    match guard.worker_no_result_nudged(session.id) {
        Ok(true) => return NoResultVerdict::SecondMiss,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(session_id = %session.id, %error, "no-result nudge lookup failed");
            return NoResultVerdict::NotApplicable;
        }
    }
    let job_running = guard
        .owner_has_running_agent_job(session.id)
        .unwrap_or(false);
    if children_at_result || job_running {
        NoResultVerdict::FirstMiss
    } else {
        NoResultVerdict::NotApplicable
    }
}

/// Insert the worker's single one-shot Resume wake. Idempotent by id.
pub(crate) async fn schedule_no_result_continuation(
    store: &Arc<Mutex<Store>>,
    session: &Session,
) -> crate::error::Result<bool> {
    let delivery = rsi_common::daemon_message::wrap(
        "worker-no-result",
        &format!(
            "{NO_RESULT_MARKER} Your turn ended without a final RESULT line while your run is still going. Wait for it in the foreground (do not end the turn), then report the final `RESULT ...` line."
        ),
    );
    let mut wake = super::harness::tools::schedule_wake::build_agent_scheduled_job(
        super::harness::tools::schedule_wake::ScheduleWakeRequest {
            message: delivery,
            in_seconds: Some(1),
            at: None,
            name: Some(format!("{NO_RESULT_WAKE_NAME_PREFIX}{}", session.id)),
            every_seconds: None,
            mode: Some("resume".to_string()),
            working_dir: session
                .sandbox_root
                .clone()
                .unwrap_or_else(|| session.working_dir.clone()),
            provider: Some(session.provider),
            model: session.model.clone(),
            project_id: session.project_id,
            origin_session_id: Some(session.id),
            watch_session_id: None,
        },
    )
    .map_err(crate::error::DaemonError::InvalidParam)?;
    wake.id = no_result_wake_id(session.id);
    let guard = store.lock().await;
    if guard.worker_no_result_nudged(session.id)? {
        return Ok(false);
    }
    guard.insert_scheduled_job(&wake)?;
    Ok(true)
}

/// Verdict for the active session about to finalize with `status`.
pub(crate) async fn verdict_for_active(
    store: &Arc<Mutex<Store>>,
    active: &Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>,
    >,
    session_id: Uuid,
    status: SessionStatus,
    assistant_output: &str,
    children_at_result: bool,
) -> NoResultVerdict {
    if status != SessionStatus::Completed {
        return NoResultVerdict::NotApplicable;
    }
    let session = active
        .read()
        .await
        .get(&session_id)
        .map(|tracked| tracked.session.clone());
    match session {
        Some(session) => {
            worker_no_result_verdict(store, &session, assistant_output, children_at_result).await
        }
        None => NoResultVerdict::NotApplicable,
    }
}

/// Schedule the single continuation for the just-finalized Completed worker.
pub(crate) async fn schedule_for_completed(
    store: &Arc<Mutex<Store>>,
    completed: &Arc<
        tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::CompletedSession>>,
    >,
    session_id: Uuid,
) {
    let terminal = completed
        .read()
        .await
        .get(&session_id)
        .map(|completed| completed.session.clone());
    let Some(terminal) = terminal else {
        return;
    };
    if terminal.status != SessionStatus::Completed {
        return;
    }
    match schedule_no_result_continuation(store, &terminal).await {
        Ok(true) => tracing::info!(
            session_id = %session_id,
            "Worker ended without RESULT while its run is live; scheduled one continuation"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            session_id = %session_id,
            %error,
            "Failed to schedule the no-result continuation"
        ),
    }
}

// The monitor future is polled on a 2 MiB worker stack and its debug frame is
// already large. These constructors are plain functions so the (large) future
// is built in their own short-lived frame and only a `Box` reaches the
// monitor's frame (#1098: an inline `Box::pin(async_fn(..))` still built the
// whole future in the monitor's frame and overflowed the stack).
type ActiveMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>>;
type CompletedMap =
    Arc<tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::CompletedSession>>>;
type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[inline(never)]
pub(crate) fn owned_children_alive_boxed(active: &ActiveMap, session_id: Uuid) -> BoxFut<'_, bool> {
    Box::pin(owned_children_alive(active, session_id))
}

#[inline(never)]
pub(crate) fn verdict_for_active_boxed<'a>(
    store: &'a Arc<Mutex<Store>>,
    active: &'a ActiveMap,
    session_id: Uuid,
    status: SessionStatus,
    assistant_output: &'a str,
    children_at_result: bool,
) -> BoxFut<'a, NoResultVerdict> {
    Box::pin(verdict_for_active(
        store,
        active,
        session_id,
        status,
        assistant_output,
        children_at_result,
    ))
}

#[inline(never)]
pub(crate) fn schedule_for_completed_boxed<'a>(
    store: &'a Arc<Mutex<Store>>,
    completed: &'a CompletedMap,
    session_id: Uuid,
) -> BoxFut<'a, ()> {
    Box::pin(schedule_for_completed(store, completed, session_id))
}
