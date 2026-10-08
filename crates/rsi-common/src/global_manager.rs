//! Global manager v0 (#872 Slice B): wire types for the operator grant and the
//! global seat's agent verbs.
//!
//! The operator grants one session (the global seat) an explicit project
//! list, a launch allowlist for the project managers (PMs) it appoints, and the
//! V2 policy body it saves for them. The seat cannot widen any of them. The
//! daemon resolves the caller from its token and checks every call.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use crate::types::{SessionProvider, SessionStatus};

/// Most projects one grant may name.
pub const GLOBAL_MANAGER_MAX_PROJECTS: usize = 64;
/// Most launch choices one grant may allow.
pub const GLOBAL_MANAGER_MAX_LAUNCHES: usize = 16;
/// Largest message the global seat or a PM may send (bytes).
pub const GLOBAL_MANAGER_MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// Largest appointment query (bytes).
pub const GLOBAL_MANAGER_MAX_QUERY_BYTES: usize = 64 * 1024;

/// The caller is not the active global seat.
pub const GLOBAL_MANAGER_NOT_SEAT: &str = "global_manager_not_seat";
/// The named project is outside the active grant.
pub const GLOBAL_PROJECT_NOT_IN_GRANT: &str = "global_project_not_in_grant";
/// The granted project has no current PM seat.
pub const GLOBAL_PROJECT_HAS_NO_MANAGER: &str = "global_project_has_no_manager";
/// The requested launch is not in the grant's `allowed_launches`.
pub const GLOBAL_LAUNCH_NOT_ALLOWED: &str = "global_launch_not_allowed";
/// The caller is not the current PM of a project in the active grant.
pub const GLOBAL_REPORT_NOT_AUTHORIZED: &str = "global_report_not_authorized";
/// No active global grant exists.
pub const GLOBAL_MANAGER_NOT_CONFIGURED: &str = "global_manager_not_configured";
/// `expected_grant_version` does not match the active grant.
pub const GLOBAL_MANAGER_STALE: &str = "global_manager_stale_grant_version";
/// A reused idempotency key carries a different request.
pub const GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT: &str = "global_manager_idempotency_conflict";
/// The request is malformed.
pub const GLOBAL_MANAGER_INVALID_REQUEST: &str = "global_manager_invalid_request";
/// Too many undelivered messages wait for one recipient.
pub const GLOBAL_MANAGER_MAILBOX_FULL: &str = "global_manager_mailbox_full";
/// #1235: a project-bound verb named a `project_id` that no manager arm of the
/// caller covers (not its own project's manager, not an area node there, and
/// not inside the active global grant).
pub const MANAGER_PROJECT_NOT_IN_SCOPE: &str = "manager_project_not_in_scope";
/// #1235 rule (c): mutations flow down. A project manager or area node may not
/// mutate the global seat or a session the global principal launched.
pub const MANAGER_TARGET_OWNED_BY_ANCESTOR: &str = "manager_target_owned_by_ancestor";
/// #1235 rule (b): one live Issue-bound worker per Issue across the chain.
pub const MANAGER_ISSUE_WORKER_ALREADY_LIVE: &str = "manager_issue_worker_already_live";
/// #1235 rule (d): the covering global grant's creation budget for the
/// project, counted over every manager principal there, is spent.
pub const MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED: &str = "manager_ancestor_allowance_exceeded";

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 128
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max
}

fn validate_launch(launch: &ManagerLaunchChoiceV2) -> Result<(), &'static str> {
    if launch.model.trim().is_empty() || launch.model.len() > 128 {
        return Err(GLOBAL_MANAGER_INVALID_REQUEST);
    }
    if launch
        .effort
        .as_deref()
        .is_some_and(|effort| effort.trim().is_empty() || effort.len() > 32)
    {
        return Err(GLOBAL_MANAGER_INVALID_REQUEST);
    }
    Ok(())
}

/// One grant row as the operator and the seat see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalManagerGrantV1 {
    pub grant_id: Uuid,
    pub grant_version: i64,
    pub seat_session_id: Uuid,
    /// `active` or `revoked`.
    pub state: String,
    pub project_ids: Vec<Uuid>,
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    pub project_policy: ManagerPolicyV2,
    pub operator_origin: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Operator-only: appoint or replace the global seat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureGlobalManagerRequestV1 {
    pub session_id: Uuid,
    pub project_ids: Vec<Uuid>,
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    pub project_policy: ManagerPolicyV2,
    /// The active grant's version, or 0 when none is active.
    pub expected_grant_version: i64,
    pub idempotency_key: String,
}

impl ConfigureGlobalManagerRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.session_id.is_nil()
            || self.expected_grant_version < 0
            || !valid_key(&self.idempotency_key)
            || self.project_ids.is_empty()
            || self.project_ids.len() > GLOBAL_MANAGER_MAX_PROJECTS
            || self.allowed_launches.is_empty()
            || self.allowed_launches.len() > GLOBAL_MANAGER_MAX_LAUNCHES
        {
            return Err(GLOBAL_MANAGER_INVALID_REQUEST);
        }
        let mut projects = self.project_ids.clone();
        projects.sort_unstable();
        projects.dedup();
        if projects.len() != self.project_ids.len() || projects.iter().any(Uuid::is_nil) {
            return Err(GLOBAL_MANAGER_INVALID_REQUEST);
        }
        for launch in &self.allowed_launches {
            validate_launch(launch)?;
        }
        self.project_policy.validate()
    }
}

/// Operator-only: read the active grant (`null` when none).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetGlobalManagerRequestV1 {}

/// Operator-only: revoke the active grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeGlobalManagerRequestV1 {
    pub expected_grant_version: i64,
    pub idempotency_key: String,
}

impl RevokeGlobalManagerRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.expected_grant_version <= 0 || !valid_key(&self.idempotency_key) {
            return Err(GLOBAL_MANAGER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// `AgentGlobalOverview {}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGlobalOverviewRequestV1 {}

/// The current PM seat of one project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalPmSeatV1 {
    pub session_id: Uuid,
    pub status: SessionStatus,
    pub provider: SessionProvider,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_fill_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub updated_at: DateTime<Utc>,
    /// The project's manager scope version (`HarnessManagerConfigV1.row_version`).
    pub scope_version: i64,
    pub pending_question: bool,
}

/// The PM's saved V2 policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalPmPolicyV1 {
    /// Live project and ancestor ceilings; absent on revoked policies or older daemons.
    #[serde(default)]
    pub effective_caps: Option<crate::portfolio_nodes::ManagerResourceCapsV1>,
    pub policy_version: i64,
    pub mode: ManagerOperatingModeV2,
    pub revoked: bool,
    pub paused: bool,
    pub capabilities: Vec<ManagerCapabilityV2>,
}

/// Issue counts of one project (archived Issues excluded).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalIssueCountsV1 {
    pub open: i64,
    pub in_progress: i64,
    /// Open Issues labelled `operator-request`.
    pub open_operator_requests: i64,
}

/// One granted project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalProjectOverviewV1 {
    pub project_id: Uuid,
    pub name: String,
    pub path: Option<String>,
    pub manager: Option<GlobalPmSeatV1>,
    pub policy: Option<GlobalPmPolicyV1>,
    pub issues: GlobalIssueCountsV1,
    pub running_sessions: i64,
    pub waiting_approval_sessions: i64,
    /// Live sessions with a pending question.
    pub pending_questions: i64,
    /// Pending tool approvals.
    pub pending_approvals: i64,
}

/// `AgentGlobalOverview` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentGlobalOverviewResultV1 {
    pub grant_version: i64,
    pub projects: Vec<GlobalProjectOverviewV1>,
}

/// Operator-only (#1213): one bounded snapshot for the TUI global manager
/// workspace. Not an agent verb: the seat reads `AgentGlobalOverview`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetGlobalManagerWorkspaceRequestV1 {}

/// The global seat's session as the operator sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalSeatSessionV1 {
    pub session_id: Uuid,
    /// The project that owns the seat session (the jump target's tab).
    pub project_id: Option<Uuid>,
    pub status: SessionStatus,
    pub provider: SessionProvider,
    pub model: Option<String>,
    pub context_fill_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub updated_at: DateTime<Utc>,
    pub pending_question: bool,
    /// #1627: earlier sessions of this seat, newest first, found by walking
    /// `continued_from` (context-cap rotations and successions). Bounded by
    /// [`SEAT_PREDECESSOR_LIMIT`]; read-only history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predecessors: Vec<SeatPredecessorV1>,
    /// More predecessors exist than `predecessors` lists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub predecessors_truncated: bool,
}

/// How many predecessors one seat snapshot lists.
pub const SEAT_PREDECESSOR_LIMIT: usize = 8;

/// One earlier session of a manager seat (#1627).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeatPredecessorV1 {
    pub session_id: Uuid,
    pub project_id: Option<Uuid>,
    pub status: SessionStatus,
    pub provider: SessionProvider,
    pub model: Option<String>,
    pub context_fill_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub updated_at: DateTime<Utc>,
}

/// One project of the workspace grant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalWorkspaceProjectV1 {
    pub overview: GlobalProjectOverviewV1,
    /// The project's manager scope (PM seat) was revoked.
    pub scope_revoked: bool,
}

/// `GetGlobalManagerWorkspace` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GlobalManagerWorkspaceV1 {
    /// The active grant, else the most recent (revoked) grant, else `None`
    /// when no global manager was ever appointed.
    pub grant: Option<GlobalManagerGrantV1>,
    /// The grant's seat session; `None` when the session no longer exists.
    pub seat: Option<GlobalSeatSessionV1>,
    pub projects: Vec<GlobalWorkspaceProjectV1>,
    /// Granted project ids whose project no longer exists.
    pub missing_project_ids: Vec<Uuid>,
}

/// `AgentGlobalSend {project_id, message, idempotency_key}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGlobalSendRequestV1 {
    pub project_id: Uuid,
    pub message: String,
    pub idempotency_key: String,
}

impl AgentGlobalSendRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.is_nil()
            || !valid_text(&self.message, GLOBAL_MANAGER_MAX_MESSAGE_BYTES)
            || !valid_key(&self.idempotency_key)
        {
            return Err(GLOBAL_MANAGER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// `AgentReportToGlobal {message, idempotency_key}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReportToGlobalRequestV1 {
    pub message: String,
    pub idempotency_key: String,
}

impl AgentReportToGlobalRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_text(&self.message, GLOBAL_MANAGER_MAX_MESSAGE_BYTES)
            || !valid_key(&self.idempotency_key)
        {
            return Err(GLOBAL_MANAGER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// Receipt of one global-manager message. The message is a durable one-shot
/// resume wake on the recipient: it is delivered at the recipient's next idle
/// boundary and wakes an idle recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalManagerMessageReceiptV1 {
    pub message_id: Uuid,
    pub project_id: Uuid,
    pub target_session_id: Uuid,
    pub deduplicated: bool,
}

/// `AgentGlobalAppointManager {project_id, launch, query, idempotency_key}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGlobalAppointManagerRequestV1 {
    pub project_id: Uuid,
    pub launch: ManagerLaunchChoiceV2,
    pub query: String,
    pub idempotency_key: String,
    /// Launch the PM in its own git worktree sandbox (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
}

impl AgentGlobalAppointManagerRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.is_nil()
            || !valid_text(&self.query, GLOBAL_MANAGER_MAX_QUERY_BYTES)
            || !valid_key(&self.idempotency_key)
        {
            return Err(GLOBAL_MANAGER_INVALID_REQUEST);
        }
        validate_launch(&self.launch)
    }
}

/// `AgentGlobalAppointManager` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGlobalAppointManagerResultV1 {
    pub session_id: Uuid,
    pub scope_version: i64,
    pub policy_version: i64,
    pub deduplicated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> ManagerLaunchChoiceV2 {
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5-5".into(),
            effort: Some("high".into()),
        }
    }

    #[test]
    fn configure_requires_projects_launches_and_a_key() {
        let mut request = ConfigureGlobalManagerRequestV1 {
            session_id: Uuid::new_v4(),
            project_ids: vec![Uuid::new_v4()],
            allowed_launches: vec![launch()],
            project_policy: ManagerPolicyV2::default(),
            expected_grant_version: 0,
            idempotency_key: "k".into(),
        };
        assert_eq!(request.validate(), Ok(()));
        request.project_ids.push(request.project_ids[0]);
        assert_eq!(request.validate(), Err(GLOBAL_MANAGER_INVALID_REQUEST));
        request.project_ids.pop();
        request.allowed_launches.clear();
        assert_eq!(request.validate(), Err(GLOBAL_MANAGER_INVALID_REQUEST));
    }

    #[test]
    fn send_and_report_require_text_and_key() {
        let send = AgentGlobalSendRequestV1 {
            project_id: Uuid::new_v4(),
            message: "status?".into(),
            idempotency_key: "k".into(),
        };
        assert_eq!(send.validate(), Ok(()));
        let report = AgentReportToGlobalRequestV1 {
            message: " ".into(),
            idempotency_key: "k".into(),
        };
        assert_eq!(report.validate(), Err(GLOBAL_MANAGER_INVALID_REQUEST));
    }
}
