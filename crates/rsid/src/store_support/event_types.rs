//! Small enums carried by bus events and model-control retry classification
//! (moved down from `reconciliation`, `stall_classifier::types` and
//! `session::types`, which re-export them).

use serde::{Deserialize, Serialize};

/// Reason a session was reconciled (transitioned by the reconciliation loop).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationReason {
    /// Process exited but session was still marked as active.
    ProcessDied,
    /// Session was active in SQLite but not in the in-memory map (or vice versa).
    StoreDesync,
    /// Session was stalled beyond threshold and auto-remediation was enabled.
    StallRemediation,
}

/// Closed whitelist of classifier verdicts. Any other string at deserialize
/// time produces a serde error and the scheduler logs + skips the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum Verdict {
    /// Agent reached natural completion. No further action needed.
    Finished,
    /// Agent is waiting for human input (pending_question present, or
    /// awaiting a decision/permission). No automated action.
    NeedsUser,
    /// Agent was mid-work and stopped without resolving. A short "continue"
    /// nudge will get it moving.
    StalledContinue,
    /// Agent appears blocked because a spawned sub-agent or team member went
    /// silent. A nudge to check on sub-agents (TaskList/TaskGet) will help.
    StalledCheckTeam,
}

impl Verdict {
    /// Human-readable lowercase label used in TUI rendering and telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::NeedsUser => "needs_user",
            Self::StalledContinue => "stalled_continue",
            Self::StalledCheckTeam => "stalled_check_team",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorBreakReason {
    Result,
    StreamClosed,
    Interrupted,
    Rotation,
    /// Session was interrupted due to stall detection (eligible for retry).
    StallTimeout,
}
