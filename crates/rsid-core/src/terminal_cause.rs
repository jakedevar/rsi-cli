//! Normalized terminal cause for every `Completed`, `Failed` and `Interrupted`
//! session transition (Issue #588).
//!
//! `sessions.stop_reason` carries one static, non-empty cause for every
//! terminal row. Provider monitors still record the provider's own stop
//! reason during the turn; this module only fills what the provider left out
//! and names what interrupted a session. The vocabulary is closed: every
//! string here is a `&'static str` (or a typed prefix plus a closed cause),
//! never free-form provider text.
//!
//! * `Completed`: the provider's own stop reason, else `completed` (or
//!   `completed:archive` when an archive requested the stop).
//! * `Failed`: the monitor's typed reason, else `terminal_failure:<c5 cause>`.
//! * `Interrupted`: `interrupted:<source>` from [`InterruptSource`], unless a
//!   provider-typed reason (`provider_error:*`) already explains the stop.
//!
//! `Store::set_session_terminal_status` (in `rsid-store`) is the one store helper
//! that writes a terminal status; it refuses an empty cause so a writer cannot
//! forget it. `Store::update_session_status` routes terminal
//! statuses through the same helper with [`default_cause`].

use rsi_common::types::SessionStatus;

/// What interrupted a session. Closed set: add a variant, never a free string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptSource {
    /// The operator halted the session (`InterruptSession` RPC / TUI).
    Operator,
    /// A manager or lead halted it (`AgentHalt`).
    ManagerHalt,
    /// A manager action (retry, replace, retarget) interrupted the predecessor.
    ManagerAction,
    /// The daemon stopped or restarted; the interrupt came from its drain.
    DaemonRestart,
    /// The daemon shut down (SIGTERM / Ctrl+C) with no deploy in progress.
    DaemonShutdown,
    /// A deploy drain interrupted the running turn.
    DeployDrain,
    /// Context rotation interrupted the predecessor turn.
    Rotation,
    /// A new continuation interrupted the still-active turn first.
    ContinueSuperseded,
    /// A graph or topology execution cancelled its node session.
    GraphCancel,
    /// The recursive DAG scheduler interrupted a live attempt.
    RecursiveDag,
    /// Model-invocation cancellation interrupted the owning session.
    ModelCancel,
    /// Launch cleanup interrupted a candidate that failed to commit.
    LaunchCleanup,
    /// A manager question cleanup interrupted the session.
    QuestionCleanup,
    /// A retry child was interrupted because its parent continued.
    RetrySuperseded,
    /// The monitor saw an interrupt with no recorded source.
    Unattributed,
}

impl InterruptSource {
    pub const fn cause(self) -> &'static str {
        match self {
            Self::Operator => "interrupted:operator",
            Self::ManagerHalt => "interrupted:manager_halt",
            Self::ManagerAction => "interrupted:manager_action",
            Self::DaemonRestart => "interrupted:daemon_restart",
            Self::DaemonShutdown => "interrupted:daemon_shutdown",
            Self::DeployDrain => "interrupted:deploy_drain",
            Self::Rotation => "interrupted:rotation",
            Self::ContinueSuperseded => "interrupted:continue_superseded",
            Self::GraphCancel => "interrupted:graph_cancel",
            Self::RecursiveDag => "interrupted:recursive_dag",
            Self::ModelCancel => "interrupted:model_cancel",
            Self::LaunchCleanup => "interrupted:launch_cleanup",
            Self::QuestionCleanup => "interrupted:question_cleanup",
            Self::RetrySuperseded => "interrupted:retry_superseded",
            Self::Unattributed => "interrupted:unattributed",
        }
    }

    pub const ALL: [Self; 15] = [
        Self::Operator,
        Self::ManagerHalt,
        Self::ManagerAction,
        Self::DaemonRestart,
        Self::DaemonShutdown,
        Self::DeployDrain,
        Self::Rotation,
        Self::ContinueSuperseded,
        Self::GraphCancel,
        Self::RecursiveDag,
        Self::ModelCancel,
        Self::LaunchCleanup,
        Self::QuestionCleanup,
        Self::RetrySuperseded,
        Self::Unattributed,
    ];
}

/// A normal end of turn with no provider-reported stop reason.
pub const COMPLETED_CAUSE: &str = "completed";
/// A turn that ended because an archive requested the stop.
pub const COMPLETED_ARCHIVE_CAUSE: &str = "completed:archive";
/// Prefix for a provider-typed stop that already explains an interrupted row.
pub const PROVIDER_ERROR_PREFIX: &str = "provider_error:";

pub const fn is_terminal_status(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
    )
}

/// Cause for a writer that has no richer evidence than the status itself.
/// `None` for statuses that are not terminal causes.
pub const fn default_cause(status: SessionStatus) -> Option<&'static str> {
    match status {
        SessionStatus::Completed => Some(COMPLETED_CAUSE),
        SessionStatus::Failed => Some("terminal_failure:unknown"),
        SessionStatus::Interrupted => Some(InterruptSource::Unattributed.cause()),
        _ => None,
    }
}

fn non_empty(reason: Option<&str>) -> Option<&str> {
    reason.map(str::trim).filter(|value| !value.is_empty())
}

/// Resolve the cause to persist with a terminal transition.
///
/// `existing` is the provider/monitor stop reason recorded for this turn.
/// `failure_cause` is the closed C5 cause string for a `Failed` decision.
/// `interrupt` is the recorded interrupt source, if any. Returns `None` only
/// for a non-terminal status.
pub fn resolve_terminal_cause(
    status: SessionStatus,
    existing: Option<&str>,
    failure_cause: Option<&str>,
    interrupt: Option<InterruptSource>,
    archive_requested: bool,
) -> Option<String> {
    let existing = non_empty(existing);
    match status {
        SessionStatus::Completed => Some(match existing {
            Some(reason) => reason.to_string(),
            None if archive_requested => COMPLETED_ARCHIVE_CAUSE.to_string(),
            None => COMPLETED_CAUSE.to_string(),
        }),
        SessionStatus::Failed => Some(match existing {
            Some(reason) => reason.to_string(),
            None => format!("terminal_failure:{}", failure_cause.unwrap_or("unknown")),
        }),
        SessionStatus::Interrupted => Some(match existing {
            // A provider-typed stop (for example the Codex usage-limit hold)
            // already says why the turn ended; keep its typed reason.
            Some(reason) if reason.starts_with(PROVIDER_ERROR_PREFIX) => reason.to_string(),
            _ => interrupt
                .unwrap_or(InterruptSource::Unattributed)
                .cause()
                .to_string(),
        }),
        _ => None,
    }
}

/// `Err` when a terminal transition would persist an empty cause.
pub fn validate_cause(cause: &str) -> Result<&str, &'static str> {
    let cause = cause.trim();
    if cause.is_empty() {
        Err("terminal transition requires a non-empty cause")
    } else {
        Ok(cause)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_interrupt_source_has_a_distinct_static_cause() {
        let mut seen = std::collections::HashSet::new();
        for source in InterruptSource::ALL {
            let cause = source.cause();
            assert!(cause.starts_with("interrupted:"), "{cause}");
            assert!(validate_cause(cause).is_ok());
            assert!(seen.insert(cause), "duplicate cause {cause}");
        }
        assert_eq!(seen.len(), InterruptSource::ALL.len());
    }

    #[test]
    fn resolution_never_yields_an_empty_cause_for_a_terminal_status() {
        for status in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
        ] {
            for existing in [None, Some(""), Some("  "), Some("end_turn")] {
                for interrupt in [None, Some(InterruptSource::Operator)] {
                    let cause = resolve_terminal_cause(status, existing, None, interrupt, false)
                        .expect("terminal status resolves");
                    assert!(validate_cause(&cause).is_ok(), "{status:?}/{existing:?}");
                }
            }
            assert!(default_cause(status).is_some());
        }
        assert!(resolve_terminal_cause(SessionStatus::Running, None, None, None, false).is_none());
        assert!(validate_cause(" ").is_err());
    }

    #[test]
    fn resolution_keeps_provider_reasons_and_names_interrupts() {
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Completed,
                Some("end_turn"),
                None,
                None,
                false
            )
            .as_deref(),
            Some("end_turn")
        );
        assert_eq!(
            resolve_terminal_cause(SessionStatus::Completed, None, None, None, false).as_deref(),
            Some("completed")
        );
        assert_eq!(
            resolve_terminal_cause(SessionStatus::Completed, None, None, None, true).as_deref(),
            Some("completed:archive")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Failed,
                Some("provider_error:codex_usage_limit"),
                Some("other_terminal_failure"),
                None,
                false
            )
            .as_deref(),
            Some("provider_error:codex_usage_limit")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Failed,
                None,
                Some("stall_timeout"),
                None,
                false
            )
            .as_deref(),
            Some("terminal_failure:stall_timeout")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Interrupted,
                Some("end_turn"),
                None,
                Some(InterruptSource::ManagerHalt),
                false
            )
            .as_deref(),
            Some("interrupted:manager_halt")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Interrupted,
                Some("provider_error:codex_usage_limit"),
                None,
                Some(InterruptSource::Operator),
                false
            )
            .as_deref(),
            Some("provider_error:codex_usage_limit")
        );
    }
}
