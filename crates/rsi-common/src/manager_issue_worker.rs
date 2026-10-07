//! `AgentManagerLaunchIssueWorker` (#1100): one manager verb launches one
//! worker bound to one Issue. The daemon creates the session through the
//! `create_session` manager action (same authority and preflight), records an
//! Issue note naming the worker, sets the Issue InProgress and arms the
//! caller's terminal watch. A bound worker may read only its own Issue.

use crate::harness_manager_v2::{
    ManagerActionReceiptV2, ManagerLaunchChoiceV2, ManagerSandboxSourceV1, text,
};
use crate::types::IssueStatus;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The largest accepted brief (the session's first prompt).
pub const MANAGER_ISSUE_WORKER_MAX_BRIEF_BYTES: usize = 24_576;

/// Stable refusal code for a malformed request.
pub const MANAGER_ISSUE_WORKER_INVALID_REQUEST: &str = "manager_issue_worker_invalid_request";
/// #1590: `review_of` names an Issue that does not exist in the project.
pub const MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE: &str =
    "manager_issue_worker_reviewed_unavailable";
/// #1254: `continue_from` names a worker that is still running.
pub const MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE: &str = "manager_issue_worker_predecessor_live";
/// #1254: `continue_from` names a worker bound to a different Issue.
pub const MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE: &str =
    "manager_issue_worker_predecessor_other_issue";
/// #1254: `continue_from` names no Issue-bound worker of the caller's project.
pub const MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE: &str =
    "manager_issue_worker_predecessor_out_of_scope";
/// #1254: the predecessor's sandbox is gone, so there is no HEAD to branch from.
pub const MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE: &str =
    "manager_issue_worker_predecessor_sandbox_unavailable";

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
    /// #1195: where the worker's sandbox branches from; omitted means a fresh
    /// `origin/rolling` base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_source: Option<ManagerSandboxSourceV1>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// #1254: the terminal worker this launch continues (its context filled
    /// and it passed the baton). The new worker branches from that worker's
    /// committed sandbox `HEAD` (uncommitted work is reported, not copied),
    /// inherits its lineage and display identity, and its brief starts with
    /// the predecessor's final message and the Issue's latest handoff.
    /// Exclusive with `sandbox_source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_from: Option<Uuid>,
    /// Explicit manager delegation of bounded QA shard jobs (#1520).
    /// Omitted is false, including on continuation; never inherited.
    #[serde(default, skip_serializing_if = "is_false")]
    pub qa_lane: bool,
    /// #1590: the implementer Issue (display number, same project) this
    /// launch reviews. The daemon copies that Issue's text and latest handoff
    /// into the worker's brief, so the bound reviewer (who may read only its
    /// own Issue) sees the complete handoff and landing filters without a
    /// manager pasting (and truncating) them. Not inherited by a continuation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_of: Option<i64>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl AgentManagerLaunchIssueWorkerRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(source) = &self.sandbox_source {
            source.validate()?;
        }
        let valid = self.issue >= 1
            && self.review_of.is_none_or(|n| n >= 1 && n != self.issue)
            && (self.review_of.is_none() || self.continue_from.is_none())
            && self.parent_epic_id.is_none_or(|id| !id.is_nil())
            && self.continue_from.is_none_or(|id| !id.is_nil())
            && (self.continue_from.is_none() || self.sandbox_source.is_none())
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
            project_id: None,
            issue: 7,
            brief: "build it".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "claude-sonnet-5-5".into(),
                effort: None,
            },
            parent_epic_id: None,
            idempotency_key: "k".into(),
            sandbox_source: None,
            continue_from: None,
            qa_lane: false,
            review_of: None,
        }
    }

    #[test]
    fn launch_provider_accepts_lowercase_and_stores_canonical() {
        let mut wire = serde_json::to_value(request()).unwrap();
        wire["launch"]["provider"] = serde_json::json!("claude");
        let parsed: AgentManagerLaunchIssueWorkerRequestV1 = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.launch.provider, SessionProvider::Claude);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["launch"]["provider"],
            "Claude"
        );
    }

    #[test]
    fn review_of_names_another_issue_and_excludes_continuation() {
        let mut review = request();
        review.review_of = Some(3);
        assert_eq!(review.validate(), Ok(()));
        let wire = serde_json::to_value(&review).unwrap();
        assert_eq!(wire["review_of"], 3);
        let old = serde_json::to_value(request()).unwrap();
        assert!(old.get("review_of").is_none());
        for bad in [Some(0), Some(7)] {
            let mut bad_review = request();
            bad_review.review_of = bad;
            assert_eq!(
                bad_review.validate(),
                Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST)
            );
        }
        let mut both = review;
        both.continue_from = Some(Uuid::from_u128(1));
        assert_eq!(both.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
    }

    #[test]
    fn qa_lane_delegation_is_manager_supplied_and_defaults_off() {
        let old = serde_json::to_value(request()).unwrap();
        let mut delegated = old.clone();
        delegated["qa_lane"] = serde_json::json!(true);
        let parsed: AgentManagerLaunchIssueWorkerRequestV1 =
            serde_json::from_value(delegated).expect("typed manager QA grant");
        assert_eq!(parsed.validate(), Ok(()));
        assert_eq!(serde_json::to_value(parsed).unwrap()["qa_lane"], true);
        let old: AgentManagerLaunchIssueWorkerRequestV1 = serde_json::from_value(old).unwrap();
        assert_eq!(serde_json::to_value(old).unwrap().get("qa_lane"), None);
        let mut malformed = serde_json::to_value(request()).unwrap();
        malformed["qa_lane"] = serde_json::json!("true");
        assert!(
            serde_json::from_value::<AgentManagerLaunchIssueWorkerRequestV1>(malformed).is_err()
        );
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
        // #1254: continue_from is a real session and replaces sandbox_source.
        let mut good = request();
        good.continue_from = Some(Uuid::new_v4());
        assert_eq!(good.validate(), Ok(()));
        let mut bad = good.clone();
        bad.sandbox_source = Some(ManagerSandboxSourceV1::Rolling {});
        assert_eq!(bad.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
        let mut bad = request();
        bad.continue_from = Some(Uuid::nil());
        assert_eq!(bad.validate(), Err(MANAGER_ISSUE_WORKER_INVALID_REQUEST));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn sandbox_source_is_externally_tagged_and_shape_checked() {
        use crate::harness_manager_v2::MANAGER_SANDBOX_SOURCE_INVALID;
        let parse = |value: serde_json::Value| {
            serde_json::from_value::<ManagerSandboxSourceV1>(value).unwrap()
        };
        assert_eq!(
            parse(serde_json::json!({"rolling": {}})),
            ManagerSandboxSourceV1::Rolling {}
        );
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            parse(serde_json::json!({"commit": sha})),
            ManagerSandboxSourceV1::Commit(sha.into())
        );
        assert_eq!(
            parse(serde_json::json!({"path": "/tmp/sandbox"})),
            ManagerSandboxSourceV1::Path("/tmp/sandbox".into())
        );
        let mut request = request();
        request.sandbox_source = Some(ManagerSandboxSourceV1::Commit(sha.into()));
        assert_eq!(request.validate(), Ok(()));
        for bad in [
            ManagerSandboxSourceV1::Commit(sha.to_uppercase()),
            ManagerSandboxSourceV1::Commit("abc".into()),
            ManagerSandboxSourceV1::Path("relative/path".into()),
        ] {
            request.sandbox_source = Some(bad);
            assert_eq!(request.validate(), Err(MANAGER_SANDBOX_SOURCE_INVALID));
        }
        // An omitted source keeps the wire form of pre-#1195 requests.
        let encoded = serde_json::to_value(super::tests::request()).unwrap();
        assert!(encoded.get("sandbox_source").is_none());
    }
}
