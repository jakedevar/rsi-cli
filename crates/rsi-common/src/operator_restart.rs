//! #1122: operator-requested quiet-point daemon restart.
//!
//! `make release-install` builds, then asks the running daemon to swap the
//! built binaries in and restart at a quiet point instead of cutting off
//! running manager and worker turns. It reuses the `AgentRequestDeploy` runner
//! (#1045); these are the operator-only wire types (not agent verbs).

use crate::agent_deploy::{DEPLOY_DEFAULT_MAX_WAIT_SECS, DEPLOY_MAX_WAIT_SECS, DeployState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const RESTART_INVALID_REQUEST: &str = "operator_restart_invalid_request";
pub const RESTART_NOTHING_PENDING: &str = "operator_restart_nothing_pending";
pub const RESTART_NOT_CANCELLABLE: &str = "operator_restart_not_cancellable";

/// `RequestOperatorRestart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestOperatorRestartRequestV1 {
    /// Absolute directory of the freshly built binaries (`rsid` at least).
    pub binaries_dir: String,
    /// Bound on the wait for a quiet point; default 900, at most 3600.
    #[serde(default)]
    pub max_wait_secs: Option<u32>,
    /// Restart without waiting for a quiet point.
    #[serde(default)]
    pub now: bool,
}

impl RequestOperatorRestartRequestV1 {
    /// # Errors
    /// `operator_restart_invalid_request`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.binaries_dir.starts_with('/')
            || self
                .max_wait_secs
                .is_some_and(|wait| wait == 0 || wait > DEPLOY_MAX_WAIT_SECS)
        {
            return Err(RESTART_INVALID_REQUEST);
        }
        Ok(())
    }

    #[must_use]
    pub fn wait_secs(&self) -> u32 {
        self.max_wait_secs.unwrap_or(DEPLOY_DEFAULT_MAX_WAIT_SECS)
    }
}

/// `GetOperatorRestart`, `CancelOperatorRestart` and `ForceOperatorRestart`
/// take no parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorRestartNoParamsV1 {}

/// The pending restart, or the outcome of the latest one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorRestartStatusV1 {
    /// A restart is staged or in flight.
    pub pending: bool,
    pub deploy_id: Option<Uuid>,
    pub state: Option<DeployState>,
    pub sha: Option<String>,
    /// RFC3339 nanos: the quiet-point wait ends here.
    pub release_by: Option<String>,
    /// The quiet gate is skipped ("restart now").
    pub forced: bool,
    /// Why the restart is still waiting (`landing_in_progress`, `job_running`,
    /// `worker_mid_turn`, `manager_mid_turn`).
    pub blockers: Vec<String>,
    /// Sessions mid-turn right now.
    pub turns_in_flight: u32,
    /// Settlement reason of a terminal restart.
    pub reason: Option<String>,
    /// The daemon is supervised, so it can restart itself (exit 75).
    pub supervised: bool,
}

impl OperatorRestartStatusV1 {
    /// One line for the TUI status bar and the install script.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if !self.pending {
            return None;
        }
        if self.state == Some(DeployState::Restarting) {
            return Some("restart in progress".to_string());
        }
        if self.forced {
            return Some("restart pending: restarting now".to_string());
        }
        let release_by = self
            .release_by
            .as_deref()
            .map(|at| format!(", release by {at}"))
            .unwrap_or_default();
        let mut waiting = Vec::new();
        if self.blockers.iter().any(|b| b == "landing_in_progress") {
            waiting.push("a landing".to_string());
        }
        if self.turns_in_flight > 0 {
            waiting.push(format!("{} turns", self.turns_in_flight));
        }
        if self.blockers.iter().any(|b| b == "job_running") {
            waiting.push("a job".to_string());
        }
        if waiting.is_empty() {
            Some(format!("restart pending: quiet, restarting{release_by}"))
        } else {
            Some(format!(
                "restart pending: waiting for {}{release_by}",
                waiting.join(" + ")
            ))
        }
    }
}
