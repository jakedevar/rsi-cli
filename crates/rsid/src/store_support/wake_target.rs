//! Wake-target liveness predicate (moved down from `scheduler`, which
//! re-exports it).

use rsi_common::types::SessionStatus;

/// Is this wake target still a LIVE session — i.e. one that may still have a
/// provider subprocess writing to its working directory?
///
/// Issue #30 / #27: an `AgentFresh` job's `working_dir` is the arming agent's
/// own sandbox worktree (bound server-side from `caller.working_dir` in
/// `handle_agent_schedule_wake`). Launching into it while the origin is live
/// puts a SECOND agent process in one worktree — two uncoordinated writers.
///
/// `SessionStatus` is `#[non_exhaustive]`, so an exhaustive match is not
/// available to this crate. The arms are therefore inverted deliberately: only
/// the statuses that are KNOWN to be settled return `false`, and the wildcard
/// returns `true`. A future status variant is treated as live and declines the
/// spawn, so a new state can never silently re-open this hazard. `Deleted` is
/// listed as not-live on purpose — it has no running process, so it carries no
/// two-writer hazard, and keeping it out of the guard preserves existing
/// behavior for deleted rows.
pub(crate) fn is_live_wake_target(status: SessionStatus) -> bool {
    match status {
        SessionStatus::Completed
        | SessionStatus::Failed
        | SessionStatus::Interrupted
        | SessionStatus::Archived
        | SessionStatus::Deleted => false,
        // Starting | Running | WaitingApproval, plus any future variant.
        _ => true,
    }
}
