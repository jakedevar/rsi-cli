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
    /// The provider child died on its own (reconciliation saw it gone); the
    /// cause carries the child's exit code or signal when known.
    ProviderExit,
    /// The monitor saw an interrupt with no recorded source. A defect
    /// signal: the stored cause also names the writer that stopped it.
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
            Self::ProviderExit => "interrupted:provider_exit",
            Self::Unattributed => "interrupted:unattributed",
        }
    }

    pub const ALL: [Self; 16] = [
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
        Self::ProviderExit,
        Self::Unattributed,
    ];
}

/// How a provider child ended: an exit code, or the signal that killed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderExit {
    Code(i32),
    Signal(i32),
}

impl ProviderExit {
    /// `exit_code=N` or `signal=SIGTERM` (`signal=N` for an unnamed signal).
    pub fn detail(self) -> String {
        match self {
            Self::Code(code) => format!("exit_code={code}"),
            Self::Signal(signal) => match signal_name(signal) {
                Some(name) => format!("signal={name}"),
                None => format!("signal={signal}"),
            },
        }
    }
}

const fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// Evidence the monitor collects for an interrupted terminal row whose
/// interrupt source may be missing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterruptEvidence {
    /// The provider child's exit status or signal, once it was observed.
    pub exit: Option<ProviderExit>,
    /// The writer that stopped the monitor with no recorded intent.
    pub writer: Option<&'static str>,
}

/// Writer name stamped on an unattributed interrupt nobody named.
pub const UNKNOWN_WRITER: &str = "finalize_session";

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
        SessionStatus::Interrupted => Some("interrupted:unattributed:store_default"),
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
/// `interrupt` is the recorded interrupt source, if any; `evidence` names the
/// provider child's exit and the stopping writer for the cases where it is
/// missing. Returns `None` only for a non-terminal status.
pub fn resolve_terminal_cause(
    status: SessionStatus,
    existing: Option<&str>,
    failure_cause: Option<&str>,
    interrupt: Option<InterruptSource>,
    evidence: InterruptEvidence,
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
            _ => interrupted_cause(interrupt, evidence),
        }),
        _ => None,
    }
}

/// `interrupted:<source>`, with the provider's exit appended for a provider
/// exit, and `interrupted:unattributed:<writer>[:<exit>]` when no source was
/// recorded, so an unattributed row is traceable to the writer that stopped it.
fn interrupted_cause(interrupt: Option<InterruptSource>, evidence: InterruptEvidence) -> String {
    let exit = evidence.exit.map(ProviderExit::detail);
    match interrupt {
        Some(InterruptSource::ProviderExit) => match exit {
            Some(detail) => format!("{}:{detail}", InterruptSource::ProviderExit.cause()),
            None => InterruptSource::ProviderExit.cause().to_string(),
        },
        Some(InterruptSource::Unattributed) | None => {
            let mut cause = format!(
                "{}:{}",
                InterruptSource::Unattributed.cause(),
                evidence.writer.unwrap_or(UNKNOWN_WRITER)
            );
            if let Some(detail) = exit {
                cause.push(':');
                cause.push_str(&detail);
            }
            cause
        }
        Some(source) => source.cause().to_string(),
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
                    let cause = resolve_terminal_cause(
                        status,
                        existing,
                        None,
                        interrupt,
                        InterruptEvidence::default(),
                        false,
                    )
                    .expect("terminal status resolves");
                    assert!(validate_cause(&cause).is_ok(), "{status:?}/{existing:?}");
                }
            }
            assert!(default_cause(status).is_some());
        }
        assert!(
            resolve_terminal_cause(
                SessionStatus::Running,
                None,
                None,
                None,
                InterruptEvidence::default(),
                false
            )
            .is_none()
        );
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
                InterruptEvidence::default(),
                false
            )
            .as_deref(),
            Some("end_turn")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Completed,
                None,
                None,
                None,
                InterruptEvidence::default(),
                false
            )
            .as_deref(),
            Some("completed")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Completed,
                None,
                None,
                None,
                InterruptEvidence::default(),
                true
            )
            .as_deref(),
            Some("completed:archive")
        );
        assert_eq!(
            resolve_terminal_cause(
                SessionStatus::Failed,
                Some("provider_error:codex_usage_limit"),
                Some("other_terminal_failure"),
                None,
                InterruptEvidence::default(),
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
                InterruptEvidence::default(),
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
                InterruptEvidence::default(),
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
                InterruptEvidence::default(),
                false
            )
            .as_deref(),
            Some("provider_error:codex_usage_limit")
        );
    }

    #[test]
    fn unattributed_interrupt_names_the_writer_and_exit_evidence() {
        let cause = |interrupt, evidence| {
            resolve_terminal_cause(
                SessionStatus::Interrupted,
                None,
                None,
                interrupt,
                evidence,
                false,
            )
            .unwrap()
        };
        assert_eq!(
            cause(None, InterruptEvidence::default()),
            "interrupted:unattributed:finalize_session"
        );
        assert_eq!(
            cause(
                None,
                InterruptEvidence {
                    exit: Some(ProviderExit::Signal(15)),
                    writer: Some("monitor_stop_signal"),
                }
            ),
            "interrupted:unattributed:monitor_stop_signal:signal=SIGTERM"
        );
        assert_eq!(
            cause(
                Some(InterruptSource::ProviderExit),
                InterruptEvidence {
                    exit: Some(ProviderExit::Signal(15)),
                    writer: None,
                }
            ),
            "interrupted:provider_exit:signal=SIGTERM"
        );
        assert_eq!(
            cause(
                Some(InterruptSource::ProviderExit),
                InterruptEvidence {
                    exit: Some(ProviderExit::Code(7)),
                    writer: None,
                }
            ),
            "interrupted:provider_exit:exit_code=7"
        );
        assert_eq!(
            cause(
                Some(InterruptSource::ProviderExit),
                InterruptEvidence::default()
            ),
            "interrupted:provider_exit"
        );
        assert_eq!(ProviderExit::Signal(64).detail(), "signal=64");
        assert!(
            default_cause(SessionStatus::Interrupted)
                .unwrap()
                .starts_with("interrupted:unattributed:")
        );
    }
}
