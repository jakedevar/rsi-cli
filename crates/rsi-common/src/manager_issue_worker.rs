//! `AgentManagerLaunchIssueWorker` (#1100): one manager verb launches one
//! worker bound to one Issue. The daemon creates the session through the
//! `create_session` manager action (same authority and preflight), records an
//! Issue note naming the worker, sets the Issue InProgress and arms the
//! caller's terminal watch. A bound worker may read only its own Issue.

use crate::harness_manager_v2::{ManagerActionReceiptV2, ManagerLaunchChoiceV2, text};
use crate::types::IssueStatus;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The largest accepted brief (the session's first prompt).
pub const MANAGER_ISSUE_WORKER_MAX_BRIEF_BYTES: usize = 24_576;

/// Stable refusal code for a malformed request.
pub const MANAGER_ISSUE_WORKER_INVALID_REQUEST: &str = "manager_issue_worker_invalid_request";

/// `AgentManagerLaunchIssueWorker {issue, brief, launch, parent_epic_id?,
/// idempotency_key}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerLaunchIssueWorkerRequestV1 {
    /// The Issue's display number in the caller's project.
    pub issue: i64,
    /// The worker's task text. The Issue text is not pasted: the worker reads
    /// its own Issue with `AgentGetIssue`.
    pub brief: String,
    pub launch: ManagerLaunchChoiceV2,
    /// The Epic the worker is created under. Optional when the manager's
    /// scope holds exactly one Epic.
    #[serde(default)]
    pub parent_epic_id: Option<Uuid>,
    pub idempotency_key: String,
}

impl AgentManagerLaunchIssueWorkerRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = self.issue >= 1
            && self.parent_epic_id.is_none_or(|id| !id.is_nil())
            && text(&self.brief, MANAGER_ISSUE_WORKER_MAX_BRIEF_BYTES).is_ok()
            && text(&self.idempotency_key, 128).is_ok()
            && self.launch.validate().is_ok();
        if valid {
            Ok(())
        } else {
            Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST)
        }
    }
}

/// How the caller's terminal watch on the worker stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerIssueWorkerWatchV1 {
    /// Armed for the caller on the live worker.
    Armed,
    /// The worker session does not exist yet: the daemon arms the watch the
    /// moment the launch is established (or on a replay of this call).
    PendingLaunch,
}

/// `AgentManagerLaunchIssueWorker` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentManagerLaunchIssueWorkerResultV1 {
    /// The `create_session` manager action receipt (poll with
    /// `AgentManagerGetAction`).
    pub action: ManagerActionReceiptV2,
    pub worker_session_id: Uuid,
    pub issue_id: Uuid,
    pub issue_display_number: i64,
    pub issue_status: IssueStatus,
    pub issue_row_version: i64,
    pub watch: ManagerIssueWorkerWatchV1,
    pub deduplicated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionProvider;

    fn request() -> AgentManagerLaunchIssueWorkerRequestV1 {
        AgentManagerLaunchIssueWorkerRequestV1 {
            issue: 7,
            brief: "build it".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-sonnet-5-5".into(),
                effort: None,
            },
            parent_epic_id: None,
            idempotency_key: "k".into(),
        }
    }

    #[test]
    fn request_validates_issue_brief_and_key() {
        assert_eq!(request().validate(), Ok(()));
        let mut bad = request();
        bad.issue = 0;
        assert_eq!(bad.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
        let mut bad = request();
        bad.brief = "  ".into();
        assert_eq!(bad.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
        let mut bad = request();
        bad.idempotency_key = String::new();
        assert_eq!(bad.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
        let mut bad = request();
        bad.parent_epic_id = Some(Uuid::nil());
        assert_eq!(bad.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
    }
}
