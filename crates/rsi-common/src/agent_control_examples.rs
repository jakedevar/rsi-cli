//! Per-verb minimal example requests and stable refusal codes for the closed
//! agent-control catalog.
//!
//! `AgentGetAuthorityCatalog {"verb": X}` serves both, so one call answers:
//! may I call this, what does a valid call look like, and what does each
//! refusal mean. Both are exhaustive `match`es over [`AgentControlVerbV1`], so
//! a new verb cannot compile without an example and a (possibly empty)
//! refusal list. The example is the schema fixture the tests validate with
//! `validate_params`. Refusals are advertisement only; the daemon stays the
//! authority boundary and a code missing here is still a valid refusal.

use crate::agent_authority_catalog::{
    AUTHORITY_CATALOG_INVALID_REQUEST, AUTHORITY_CATALOG_UNKNOWN_VERB,
};
use crate::agent_control_schema::AgentControlVerbV1;
use crate::agent_coordination::{
    AgentArchiveErrorCodeV1, AgentContinueErrorCodeV1, AgentMessageErrorCodeV1,
};
use crate::agent_deploy::{
    DEPLOY_CAPABILITY_REQUIRED, DEPLOY_EXECUTE_REQUIRED, DEPLOY_IN_PROGRESS, DEPLOY_KEY_CONFLICT,
    DEPLOY_NEEDS_SUPERVISOR, DEPLOY_NOT_AUTHORIZED, DEPLOY_SHA_INVALID, DEPLOY_SHA_MISMATCH,
    DEPLOY_TARGET_MISMATCH,
};
use crate::agent_jobs::{
    JOB_DIR_NOT_ALLOWED, JOB_INVALID_PARAMS, JOB_INVALID_REQUEST, JOB_KEY_CONFLICT,
    JOB_KEY_INVALID, JOB_KIND_NOT_AUTHORIZED, JOB_KIND_UNSUPPORTED, JOB_LAUNCH_FAILED,
    JOB_NAME_INVALID, JOB_NOT_FOUND, JOB_PLATFORM_UNSUPPORTED,
};
use crate::agent_provider_status::PROVIDER_STATUS_UNKNOWN_PROVIDER;
use crate::agent_session_events::AGENT_READ_EVENTS_SCOPE_DENIED;
use crate::global_manager::{
    GLOBAL_LAUNCH_NOT_ALLOWED, GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT, GLOBAL_MANAGER_MAILBOX_FULL,
    GLOBAL_MANAGER_NOT_SEAT, GLOBAL_PROJECT_HAS_NO_MANAGER, GLOBAL_PROJECT_NOT_IN_GRANT,
    GLOBAL_REPORT_NOT_AUTHORIZED,
};
use crate::manager_tier_routing::{
    MANAGER_TARGET_NOT_DESCENDANT, MANAGER_TIER_IDEMPOTENCY_CONFLICT, MANAGER_TIER_MAILBOX_FULL,
    MANAGER_TIER_NOT_NODE_SEAT, MANAGER_TIER_TARGET_UNKNOWN, MANAGER_TIER_TARGET_VACANT,
};
use crate::portfolio_delegation::{
    MANAGER_CHILD_OPERATOR_GRANTED, MANAGER_DIRECT_REPORT_CAP, MANAGER_NODE_NOT_IN_SCOPE,
    MANAGER_SCOPE_NOT_NARROWED,
};
use crate::portfolio_nodes::{
    MANAGER_ALLOWANCE_EXCEEDED, MANAGER_CAPABILITY_WIDENED, MANAGER_NODE_ROOT_OPERATOR_ONLY,
    MANAGER_NODE_STALE, MANAGER_SCOPE_OVERLAP, PORTFOLIO_IDEMPOTENCY_CONFLICT,
};
use crate::rolling_queue::{
    QUEUE_DISABLED, QUEUE_DUPLICATE_SOURCE, QUEUE_FILTER_INVALID, QUEUE_FILTER_MATCHES_NO_TESTS,
    QUEUE_GATE_TIMEOUT, QUEUE_KEY_INVALID, QUEUE_NOT_AUTHORIZED, QUEUE_REGATE_EXHAUSTED,
    QUEUE_SOURCE_INVALID,
};
use crate::rpc::AgentIssueErrorCodeV1;
use crate::satellite_dispatch::{
    SATELLITE_MESSAGE_INVALID, SATELLITE_MESSAGE_KEY_CONFLICT, SATELLITE_MESSAGE_KEY_INVALID,
    SATELLITE_MESSAGE_QUEUE_FULL, SATELLITE_REPORT_INVALID, SATELLITE_REPORT_NOT_AUTHORIZED,
    SATELLITE_REPORT_QUEUE_FULL, SATELLITE_TARGET_NOT_AUTHORIZED,
};
use crate::wake_predicate::{
    WAKE_WHEN_CAP_REACHED, WAKE_WHEN_FIELD_MISPLACED, WAKE_WHEN_JOB_NOT_FOUND,
    WAKE_WHEN_PREDICATE_INVALID, WAKE_WHEN_TIMEOUT_INVALID, WAKE_WHEN_TIMING_UNSUPPORTED,
};
use serde_json::Value;

/// One stable refusal a verb can return and what the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentControlRefusalV1 {
    pub code: &'static str,
    pub next_action: &'static str,
}

const fn refusal(code: &'static str, next_action: &'static str) -> AgentControlRefusalV1 {
    AgentControlRefusalV1 { code, next_action }
}

impl AgentControlVerbV1 {
    /// One minimal valid example request (the schema fixture): every required
    /// field, placeholder ids, no caller identity.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn example(self) -> Value {
        let verb = self;
        let issue = "5d73c05d-1040-49f7-92ab-0123456789ab";
        match verb {
            AgentControlVerbV1::SpawnChild => serde_json::json!({
                "kind": "Task", "provider": "Codex", "model": null,
                "agent_role": "Reviewer", "query": "inspect", "tags": ["schema"],
                "idempotency_key": "spawn-v1"
            }),
            AgentControlVerbV1::ReserveSuccessor => serde_json::json!({
                "kind": "Task", "query": "continue", "idempotency_key": "successor-v1"
            }),
            AgentControlVerbV1::GetProgress => serde_json::json!({"session_ids": []}),
            AgentControlVerbV1::SendMessage => serde_json::json!({
                "target_session_id": issue, "message": "done", "idempotency_key": "mail-v1"
            }),
            AgentControlVerbV1::GetStatus | AgentControlVerbV1::Halt => serde_json::json!({}),
            AgentControlVerbV1::ContinueChild => serde_json::json!({
                "target_session_id": issue, "query": "resume stage",
                "expected_tip_session_id": issue, "expected_event_sequence": 7
            }),
            AgentControlVerbV1::ArchiveChild => serde_json::json!({
                "target_session_id": issue,
                "expected_tip_session_id": issue, "expected_event_sequence": 7
            }),
            AgentControlVerbV1::ScheduleWake => serde_json::json!({
                "message": "continue", "in_seconds": 1, "mode": "resume"
            }),
            AgentControlVerbV1::CancelWake => serde_json::json!({"name": "safety-net"}),
            AgentControlVerbV1::ListWakes => {
                serde_json::json!({"include_disabled": true, "limit": 8})
            }
            AgentControlVerbV1::CreateIssue => serde_json::json!({
                "title": "Follow-up", "idempotency_key": "issue-v1"
            }),
            AgentControlVerbV1::ListIssues => serde_json::json!({}),
            AgentControlVerbV1::GetIssue => serde_json::json!({"issue_id": issue}),
            AgentControlVerbV1::UpdateIssue => serde_json::json!({
                "issue_id": issue, "expected_row_version": 1,
                "idempotency_key": "update-v1", "title": "Revised"
            }),
            AgentControlVerbV1::UpdateIssueStatus => serde_json::json!({
                "issue_id": issue, "status": "Closed", "expected_row_version": 1,
                "idempotency_key": "status-v1"
            }),
            AgentControlVerbV1::ArchiveIssue => serde_json::json!({
                "issue_id": issue, "expected_row_version": 1,
                "idempotency_key": "archive-v1"
            }),
            AgentControlVerbV1::RestoreIssue => serde_json::json!({
                "issue_id": issue, "expected_row_version": 1,
                "idempotency_key": "restore-v1"
            }),
            AgentControlVerbV1::ListIssueEvents => serde_json::json!({
                "issue_id": issue, "after_sequence": 0, "limit": 64
            }),
            AgentControlVerbV1::ManagerInspect => serde_json::json!({}),
            AgentControlVerbV1::ManagerUpdate => {
                serde_json::json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"update","change":{"update":"handoff","summary":"Ready","next_actions":[]}})
            }
            AgentControlVerbV1::SubmitReviewReceipt => serde_json::json!({
                "assignment_id": issue,
                "verdict": "accepted",
                "findings": [],
                "idempotency_key": "review-receipt-v1"
            }),
            AgentControlVerbV1::ManagerControl => {
                serde_json::json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"control","operation":{"action":"create_session","parent_id":issue,"kind":"Task","query":"Build the change.","launch":{"provider":"Claude","model":"claude-sonnet-5-5","effort":"high"},"sandbox_source":{"path":"/home/you/.rsi/sandboxes/your-session-id"}}})
            }
            AgentControlVerbV1::ManagerPrepareControl => {
                serde_json::json!({"operation":{"action":"resume_lead","epic_id":issue,"message":"continue"}})
            }
            AgentControlVerbV1::ManagerCommitPreparedControl => {
                serde_json::json!({"prepared_id":issue,"target_digest":format!("sha256:{}", "a".repeat(64)),"idempotency_key":"commit-prepared"})
            }
            AgentControlVerbV1::ManagerGetAction => {
                serde_json::json!({"operation_id":issue})
            }
            AgentControlVerbV1::ManagerLaunchIssueWorker => serde_json::json!({
                "issue": 1100, "brief": "Build the Issue you are bound to; read it with AgentGetIssue.",
                "launch": {"provider": "Claude", "model": "claude-sonnet-5-5", "effort": "high"},
                "parent_epic_id": issue, "idempotency_key": "launch-issue-1100-worker-1",
                "sandbox_source": {"rolling": {}}
            }),
            AgentControlVerbV1::ManagerProgress => serde_json::json!({}),
            AgentControlVerbV1::ManagerInbox => serde_json::json!({
                "after_sequence": 0, "limit": 32, "request_id": issue
            }),
            AgentControlVerbV1::ManagerSend => serde_json::json!({
                "epic_id": issue, "message": "Evidence?", "idempotency_key": "manager-send-v1", "informational": true
            }),
            AgentControlVerbV1::ManagerReply => serde_json::json!({
                "request_id": issue, "message": "Tests passed", "idempotency_key": "manager-reply-v1", "still_running": true
            }),
            AgentControlVerbV1::ManagerNotify => serde_json::json!({
                "message": "Checks passed", "idempotency_key": "manager-notify-v1"
            }),
            AgentControlVerbV1::ManagerWorkView => serde_json::json!({
                "work_key": null, "after_work_key": "alpha", "limit": 8
            }),
            AgentControlVerbV1::ManagerDelegateNode => serde_json::json!({
                "node_id": null, "parent_node_id": issue,
                "seat_root_session_id": issue,
                "selector": {"mode":"selected","group_ids":[],"epic_ids":[issue]},
                "grant": {"capabilities":[],"allowed_launches":[],"allowance":{
                    "max_created_containers":0,"max_created_sessions":0,
                    "max_active_sessions":4,"max_build_slots":0,"max_disk_gib":0,
                    "provider_limits":[],"max_spend_usd":null},"max_direct_reports":1},
                "policy": crate::harness_manager_v2::ManagerPolicyV2::default(),
                "expected_parent_grant_version":1,"expected_parent_policy_version":1,
                "expected_parent_authority_epoch":1,"expected_node_grant_version":0,
                "idempotency_key":"delegate-v1"
            }),
            AgentControlVerbV1::ManagerEscalate => serde_json::json!({
                "subject_id":issue,"reason":"Sibling scope conflict",
                "route":{"kind":"parent"},
                "expected_source_authority_epoch":1,"expected_source_grant_version":1,
                "expected_target_authority_epoch":1,"expected_target_grant_version":1,
                "expected_target_session_id":issue,"idempotency_key":"escalate-v1"
            }),
            AgentControlVerbV1::ManagerListEscalations => serde_json::json!({}),
            AgentControlVerbV1::GetAuthorityCatalog => {
                serde_json::json!({"verb":"AgentSpawnChild"})
            }
            AgentControlVerbV1::ManagerResolveEscalation => serde_json::json!({
                "escalation_id":issue,"expected_version":1,
                "expected_target_authority_epoch":1,"expected_target_grant_version":1,
                "expected_target_session_id":issue,"ruling":"Proceed",
                "idempotency_key":"resolve-v1"
            }),
            AgentControlVerbV1::TopologyUpsert => serde_json::json!({
                "name": "review-loop", "scope": "epic", "epic_id": issue,
                "definition": {"nodes": [{"id": "a", "kind": "Task", "label": "A"}], "edges": []},
                "expected_revision": null, "validate_only": true, "idempotency_key": "upsert-v1"
            }),
            AgentControlVerbV1::TopologyList => serde_json::json!({
                "scope": "epic", "epic_id": issue, "include_executions": true,
                "cursor": null, "limit": 8
            }),
            AgentControlVerbV1::TopologyExecute => serde_json::json!({
                "topology_id": issue, "expected_digest": format!("sha256:{}", "a".repeat(64)),
                "epic_id": issue, "inputs": {}, "base_commit": null, "idempotency_key": "run-v1"
            }),
            AgentControlVerbV1::TopologyGetExecution => serde_json::json!({
                "execution_id": issue, "after_sequence": 0, "limit": 64
            }),
            AgentControlVerbV1::TopologyInterrupt => serde_json::json!({
                "execution_id": issue, "expected_row_version": 2, "idempotency_key": "stop-v1"
            }),
            AgentControlVerbV1::SubmitJob => serde_json::json!({
                "kind": "build", "params": {"command": "check", "workspace": true},
                "name": "check", "idempotency_key": "job-v1"
            }),
            AgentControlVerbV1::GetJob => serde_json::json!({"job_id": issue}),
            AgentControlVerbV1::ListJobs => serde_json::json!({"limit": 10}),
            AgentControlVerbV1::CancelJob => serde_json::json!({"job_id": issue}),
            AgentControlVerbV1::EnqueueLandingSource => serde_json::json!({
                "source_commit": "0123456789abcdef0123456789abcdef01234567",
                "test_filters": ["rsid=rolling_queue"], "idempotency_key": "enqueue-v1"
            }),
            AgentControlVerbV1::ReadSessionEvents => serde_json::json!({
                "session_id": issue, "after_sequence": 0, "limit": 20,
                "event_types": ["Message"], "max_bytes": 32768,
                "final_message_full": true
            }),
            AgentControlVerbV1::GetProviderStatus => serde_json::json!({"provider": "openrouter"}),
            AgentControlVerbV1::SendSatelliteMessage => serde_json::json!({
                "peer_id": issue, "remote_session_id": issue,
                "message": "status?", "idempotency_key": "sat-msg-v1"
            }),
            AgentControlVerbV1::ReportToHub => serde_json::json!({
                "kind": "result", "text": "RESULT abc123 issue=#1 status=green"
            }),
            AgentControlVerbV1::GetDaemonInfo => serde_json::json!({}),
            AgentControlVerbV1::QueryFailureSignatures => serde_json::json!({
                "test_id": "session::launch::tests::example_test"
            }),
            AgentControlVerbV1::GlobalOverview => serde_json::json!({}),
            AgentControlVerbV1::ManagerOverview => serde_json::json!({}),
            AgentControlVerbV1::GlobalSend => serde_json::json!({
                "project_id": issue, "message": "Land #872 and report back.",
                "idempotency_key": "gm-rsi-route-1"
            }),
            AgentControlVerbV1::GlobalAppointManager => serde_json::json!({
                "project_id": issue,
                "launch": {"provider": "Claude", "model": "claude-opus-5-5", "effort": "high"},
                "query": "You are the project manager of this project. Call AgentGetAuthorityCatalog {} first.",
                "idempotency_key": "gm-rsi-appoint-1"
            }),
            AgentControlVerbV1::ReportToGlobal => serde_json::json!({
                "message": "Landed #872 on rolling; no gate pending.",
                "idempotency_key": "pm-report-1"
            }),
            AgentControlVerbV1::ReportUp => serde_json::json!({
                "message": "Landed #1238 on rolling; no gate pending.",
                "idempotency_key": "report-up-1"
            }),
            AgentControlVerbV1::SendDown => serde_json::json!({
                "target": {"kind": "project", "project_id": issue},
                "message": "Land #1238 and report back with AgentReportUp.",
                "idempotency_key": "send-down-1"
            }),
            AgentControlVerbV1::ManagerAppointChild => serde_json::json!({
                "target": {"kind": "project", "project_id": issue},
                "launch": {"provider": "Claude", "model": "claude-opus-5-5", "effort": "high"},
                "query": "You are the project manager of this project. Call AgentGetAuthorityCatalog {} first.",
                "idempotency_key": "node-appoint-pm-1"
            }),
            AgentControlVerbV1::ManagerRevokeChild => serde_json::json!({
                "node_id": issue, "expected_grant_version": 7,
                "idempotency_key": "node-revoke-child-1"
            }),
            AgentControlVerbV1::RequestDeploy => serde_json::json!({
                "sha": "0123456789abcdef0123456789abcdef01234567",
                "binaries_dir": "/tmp/bin", "idempotency_key": "deploy-v1"
            }),
            AgentControlVerbV1::TopologyResolveAttempt => serde_json::json!({
                "execution_id": issue, "attempt_id": issue, "action": "discard",
                "expected_row_version": 3, "idempotency_key": "discard-v1",
                "confirm_preserved_commit": "0123456789abcdef0123456789abcdef01234567"
            }),
        }
    }
}

// Typed families shared by several verbs.

/// #1235: a `project_id` outside every manager arm of the caller.
const MANAGER_PROJECT_NOT_IN_SCOPE: AgentControlRefusalV1 = refusal(
    crate::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE,
    "name a project in your global grant, or omit project_id for your own project",
);

/// The refusals every Issue verb shares, then any verb-specific extras.
macro_rules! issue_refusals {
    ($($extra:expr),* $(,)?) => {
        &[
        MANAGER_PROJECT_NOT_IN_SCOPE,
        refusal(
            AgentIssueErrorCodeV1::InvalidRequest.as_str(),
            "correct the request fields and retry",
        ),
        refusal(
            AgentIssueErrorCodeV1::AuthorityDenied.as_str(),
            "use the current lead of the Issue project's owning Epic; a worker launched for an Issue may only append to that Issue's body while its binding is live",
        ),
        refusal(
            AgentIssueErrorCodeV1::NotFoundInScope.as_str(),
            "verify the Issue id from the current owning Epic project",
        ),
        refusal(
            AgentIssueErrorCodeV1::IdempotencyConflict.as_str(),
            "retry the original semantic request or choose a new idempotency_key",
        ),
        refusal(
            AgentIssueErrorCodeV1::StaleVersion.as_str(),
            "refresh the Issue and retry with its row_version",
        ),
        refusal(
            AgentIssueErrorCodeV1::NoSemanticChange.as_str(),
            "make a semantic change before retrying",
        ),
        refusal(
            AgentIssueErrorCodeV1::InvalidTransition.as_str(),
            "use a lifecycle transition allowed from the current Issue status",
        ),
        refusal(
            AgentIssueErrorCodeV1::Archived.as_str(),
            "restore the Issue before updating it",
        ),
        refusal(
            AgentIssueErrorCodeV1::NotArchived.as_str(),
            "archive the terminal Issue before restoring it",
        ),
        refusal(
            AgentIssueErrorCodeV1::StorageFailure.as_str(),
            "retry later; the Issue mutation did not commit",
        ),
            $($extra),*
        ]
    };
}

const ISSUE_REFUSALS: &[AgentControlRefusalV1] = issue_refusals!();

/// `AgentUpdateIssue` also serves an Issue-bound worker's body append (#1284):
/// each way that arm can refuse is its own code (#1545).
const UPDATE_ISSUE_REFUSALS: &[AgentControlRefusalV1] = issue_refusals!(
    refusal(
        AgentIssueErrorCodeV1::BoundIssueBindingNotLive.as_str(),
        "your Issue binding ended or was superseded by a later launch of the Issue; report through your parent instead of updating the Issue",
    ),
    refusal(
        AgentIssueErrorCodeV1::BoundIssueWrongIssue.as_str(),
        "a bound worker may update only the Issue it was launched for; omit project_id or name that Issue",
    ),
    refusal(
        AgentIssueErrorCodeV1::BoundIssueFieldNotAllowed.as_str(),
        "send only body, expected_row_version and idempotency_key; omit title, labels, priority and assignee",
    ),
    refusal(
        AgentIssueErrorCodeV1::BoundIssueNotAppendOnly.as_str(),
        "re-read the Issue with AgentGetIssue and send its current body byte for byte followed by your appended text",
    ),
);

const MESSAGE_REFUSALS: &[AgentControlRefusalV1] = &[
    refusal(
        AgentMessageErrorCodeV1::TargetNotAuthorized.as_str(),
        "send only to your own reserved or direct child, or to a child of an Epic you lead",
    ),
    refusal(
        AgentMessageErrorCodeV1::IdempotencyConflict.as_str(),
        "retry with the original target, message, and expiry, or send under a new idempotency key",
    ),
    refusal(
        AgentMessageErrorCodeV1::TargetQueueFull.as_str(),
        "wait for the target to drain its pending mail before sending again",
    ),
    refusal(
        AgentMessageErrorCodeV1::OwnerQueueFull.as_str(),
        "wait for your outstanding mail to settle before sending again",
    ),
    refusal(
        AgentMessageErrorCodeV1::PayloadTooLarge.as_str(),
        "reduce the message to at most 16384 bytes and resend under the same idempotency key",
    ),
    refusal(
        AgentMessageErrorCodeV1::TargetUnknown.as_str(),
        "spawn the child first, then send to the returned child_session_id",
    ),
    refusal(
        AgentMessageErrorCodeV1::ProviderUnsupported.as_str(),
        "this target's provider cannot accept delivered mail; use a different child",
    ),
    refusal(
        AgentMessageErrorCodeV1::TargetTerminal.as_str(),
        "continue the child or target its current live successor before sending",
    ),
];

const CONTINUE_REFUSALS: &[AgentControlRefusalV1] = &[
    refusal(
        AgentContinueErrorCodeV1::InvalidRequest.as_str(),
        "correct the request fields using the AgentContinueChild schema before retrying",
    ),
    refusal(
        AgentContinueErrorCodeV1::TargetNotAuthorized.as_str(),
        "continue only your own direct child, or a child of an Epic you lead",
    ),
    refusal(
        AgentContinueErrorCodeV1::TargetUnknown.as_str(),
        "spawn the child first, then continue the returned child_session_id",
    ),
    refusal(
        AgentContinueErrorCodeV1::SelfContinuationDenied.as_str(),
        "target a child; use AgentScheduleWake with mode resume to continue yourself",
    ),
    refusal(
        AgentContinueErrorCodeV1::StaleContinuation.as_str(),
        "adopt the observed cursor, re-decide whether the continuation still applies, then retry",
    ),
    refusal(
        AgentContinueErrorCodeV1::ProviderUnsupported.as_str(),
        "this target's provider cannot continue under its own session id; spawn a successor instead",
    ),
    refusal(
        AgentContinueErrorCodeV1::ResumeUnavailableTaskUnresolved.as_str(),
        "restore a durable task prompt before retrying",
    ),
    refusal(
        AgentContinueErrorCodeV1::IdempotencyConflict.as_str(),
        "retry with the original request or use a new idempotency_key",
    ),
    refusal(
        AgentContinueErrorCodeV1::RelaunchAbandoned.as_str(),
        "retry with a new idempotency_key",
    ),
    refusal(
        AgentContinueErrorCodeV1::RelaunchInProgress.as_str(),
        "wait for the open relaunch request to settle before making a new decision",
    ),
    refusal(
        AgentContinueErrorCodeV1::DeployDraining.as_str(),
        "a deploy is waiting for its quiet point; retry the same request after it settles (see AgentGetDaemonInfo deploy_drain.release_by)",
    ),
    refusal(
        AgentContinueErrorCodeV1::ContinuationFailed.as_str(),
        "inspect the target with AgentGetProgress before retrying the continuation",
    ),
];

const ARCHIVE_REFUSALS: &[AgentControlRefusalV1] = &[
    refusal(
        AgentArchiveErrorCodeV1::InvalidRequest.as_str(),
        "correct the request fields using the AgentArchiveChild schema before retrying",
    ),
    refusal(
        AgentArchiveErrorCodeV1::SelfArchiveDenied.as_str(),
        "target a child; a session cannot archive itself",
    ),
    refusal(
        AgentArchiveErrorCodeV1::TargetUnknown.as_str(),
        "read AgentGetProgress and archive a child_session_id it reports",
    ),
    refusal(
        AgentArchiveErrorCodeV1::TargetNotAuthorized.as_str(),
        "archive only a child of an Epic you currently lead; ask that lead otherwise",
    ),
    refusal(
        AgentArchiveErrorCodeV1::StaleArchive.as_str(),
        "adopt the observed cursor, re-decide whether the archive still applies, then retry",
    ),
    refusal(
        AgentArchiveErrorCodeV1::TargetNotTerminal.as_str(),
        "wait for the child to finish, or halt it, before archiving",
    ),
    refusal(
        AgentArchiveErrorCodeV1::TargetNotLeaf.as_str(),
        "archive a leaf child; containers are operator-managed",
    ),
    refusal(
        AgentArchiveErrorCodeV1::TargetIsLead.as_str(),
        "the target leads a container; lead replacement belongs to the manager",
    ),
    refusal(
        AgentArchiveErrorCodeV1::RecoveryOwnerHeld.as_str(),
        "a human, operator or recovery owner holds the child; leave it for that owner",
    ),
    refusal(
        AgentArchiveErrorCodeV1::LiveContinuation.as_str(),
        "a continuation of the child is live or pending; let it settle before archiving",
    ),
    refusal(
        AgentArchiveErrorCodeV1::ReviewSourceSealed.as_str(),
        "the child authors review work without a verdict; wait for the verdict before archiving",
    ),
    refusal(
        AgentArchiveErrorCodeV1::ArchiveFailed.as_str(),
        "inspect the child with AgentGetProgress before retrying the archive",
    ),
];

const MANAGER_INVALID_REQUEST: AgentControlRefusalV1 = refusal(
    "manager_invalid_request",
    "correct the request fields using the verb's schema; refresh the fence with AgentManagerInspect",
);
const MANAGER_IDEMPOTENCY_CONFLICT: AgentControlRefusalV1 = refusal(
    "manager_idempotency_conflict",
    "replay only identical content under a key; use a new key after a rescope or a changed request",
);
const MANAGER_CAPABILITY_DENIED: AgentControlRefusalV1 = refusal(
    "manager_v2_capability_denied",
    "the operator has not granted this capability; ask the operator, do not retry",
);
const MANAGER_PAUSED: AgentControlRefusalV1 = refusal(
    "manager_v2_paused",
    "the project, Epic or manager is paused; only the operator clears a pause",
);
const MANAGER_HUMAN_OR_RECOVERY_OWNER: AgentControlRefusalV1 = refusal(
    "manager_v2_human_or_recovery_owner",
    "a human, recovery owner or your own enabled resume wake holds this; retire the wake or wait",
);

const SPAWN_REFUSALS: &[AgentControlRefusalV1] = &[
    refusal(
        "agent_spawn_rejected:NotLead",
        "only the current lead of the owning Epic may spawn; ask that lead or the manager",
    ),
    refusal(
        "agent_spawn_rejected:EmitterNotLeaf",
        "Group and Epic containers never spawn; spawn from a leaf session",
    ),
    refusal(
        "agent_spawn_rejected:DepthLimitExceeded",
        "the parent chain is too deep; do the work in an existing child or reserve a successor",
    ),
    refusal(
        "agent_spawn_rejected:RateLimited",
        "the Epic's spawn rate limit is hit; wait and retry the same idempotency_key",
    ),
    refusal(
        "agent_spawn_rejected:IllegalChildKind",
        "the kind is not a legal child of an Epic; pick a leaf kind",
    ),
    refusal(
        "agent_spawn_rejected:UnsupportedChildEffort",
        "use an effort inside the selected model's ladder, or omit effort",
    ),
    refusal(
        "agent_spawn_rejected:OrchestrationEscalationDenied",
        "the child's tier or effort exceeds the orchestration guardrail; request a lower tier or effort",
    ),
    refusal(
        "agent_spawn_rejected:ToolPolicyProviderUnsupported",
        "your Harness tool policy is inherited; pick a provider that can enforce it",
    ),
    refusal(
        "agent_spawn_rejected:ProviderProfileRefused",
        "the operator provider profile is aws_only; launch Claude on a Bedrock Claude model id ([<geo>.]anthropic.claude-*)",
    ),
];

const JOB_REFUSALS: &[AgentControlRefusalV1] = &[
    refusal(
        crate::agent_jobs::JOB_SANDBOX_SESSION_LIVE,
        "sandbox_session_id must name a terminal session; wait for it to finish",
    ),
    refusal(
        JOB_INVALID_REQUEST,
        "correct the request fields using the AgentSubmitJob schema and retry",
    ),
    refusal(
        JOB_INVALID_PARAMS,
        "fix the kind-specific params; see the schema description for the kind",
    ),
    refusal(
        crate::agent_jobs::JOB_TIMEOUT_NOT_AUTHORIZED,
        "ordinary test raises need the manager/Epic lead; QA shard jobs need a live unsuperseded manager Issue-launch binding with explicit qa_lane:true delegation or manager/Epic lead",
    ),
    refusal(
        crate::agent_jobs::JOB_QA_LANE_LIMIT,
        "wait for or cancel one of your two running QA shard jobs before submitting another",
    ),
    refusal(
        crate::agent_jobs::JOB_QA_LANE_SHA_MISMATCH,
        "pin qa_lane.sha to the owner sandbox submission-time HEAD and retry; probe failure also refuses; dirtiness is not checked",
    ),
    refusal(
        crate::agent_jobs::JOB_RECIPE_NOT_ALLOWED,
        "declare this recipe in the worktree .rsi/jobs.toml before submitting it",
    ),
    refusal(
        crate::agent_jobs::JOB_RECIPE_INVALID,
        "fix .rsi/jobs.toml: version 1, at most 64 recipes, just/make targets, timeout and CPU caps; keep the manifest inside the worktree",
    ),
    refusal(JOB_NAME_INVALID, "use a name of at most 80 bytes"),
    refusal(
        JOB_KEY_INVALID,
        "use an idempotency_key of at most 128 bytes without NUL",
    ),
    refusal(JOB_KIND_UNSUPPORTED, "use a kind the schema enumerates"),
    refusal(
        JOB_PLATFORM_UNSUPPORTED,
        "macOS supports package test, build and recipe jobs; use Linux for shard, candidate-receipt, landing and cloud jobs; other operating systems have no durable backend",
    ),
    refusal(
        JOB_DIR_NOT_ALLOWED,
        "run the job from your own sandbox directory",
    ),
    refusal(
        JOB_KIND_NOT_AUTHORIZED,
        "landing, cloud_gate and cloud_sweep jobs are for the appointed manager or an Epic lead; use build or test",
    ),
    refusal(
        JOB_KEY_CONFLICT,
        "retry with the original request or use a new idempotency_key",
    ),
    refusal(
        JOB_LAUNCH_FAILED,
        "inspect with AgentGetJob and resubmit under a new idempotency_key",
    ),
];

impl AgentControlVerbV1 {
    /// Stable refusal codes this verb returns, each with the caller's next
    /// step. Empty means the verb has no closed typed refusal family yet: its
    /// refusals are still real, but surface as free-text `InvalidParam`
    /// messages that are not pinned.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn refusals(self) -> &'static [AgentControlRefusalV1] {
        match self {
            Self::GetAuthorityCatalog => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        AUTHORITY_CATALOG_INVALID_REQUEST,
                        "send {} or {\"verb\": \"<name>\"} only",
                    ),
                    refusal(
                        AUTHORITY_CATALOG_UNKNOWN_VERB,
                        "name an Agent* method, rsi_control_* tool or its mcp__rsi-agent__ spelling; call with {} for the list",
                    ),
                ];
                R
            }
            Self::SpawnChild => SPAWN_REFUSALS,
            Self::SendMessage => MESSAGE_REFUSALS,
            Self::ContinueChild => CONTINUE_REFUSALS,
            Self::ArchiveChild => ARCHIVE_REFUSALS,
            Self::GetProgress => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        "agent_progress_invalid_request",
                        "send at most 256 non-nil session_ids, or none for the whole scope",
                    ),
                    refusal(
                        "cohort_too_large",
                        "subdivide the orchestration topology and request at most 256 child session_ids",
                    ),
                ];
                R
            }
            Self::ScheduleWake => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        WAKE_WHEN_PREDICATE_INVALID,
                        "set exactly one when predicate: jobs_terminal or sha_on_rolling",
                    ),
                    refusal(
                        WAKE_WHEN_JOB_NOT_FOUND,
                        "name only job ids you own, submitted through AgentSubmitJob",
                    ),
                    refusal(
                        WAKE_WHEN_TIMEOUT_INVALID,
                        "use a timeout_seconds inside the schema bounds",
                    ),
                    refusal(
                        WAKE_WHEN_CAP_REACHED,
                        "cancel an enabled wake with AgentCancelWake or reuse a wake name",
                    ),
                    refusal(
                        WAKE_WHEN_FIELD_MISPLACED,
                        "when and timeout_seconds are valid only with mode when",
                    ),
                    refusal(
                        WAKE_WHEN_TIMING_UNSUPPORTED,
                        "a when wake is daemon-timed; drop in_seconds and at",
                    ),
                ];
                R
            }
            Self::CreateIssue
            | Self::ListIssues
            | Self::GetIssue
            | Self::UpdateIssueStatus
            | Self::ArchiveIssue
            | Self::RestoreIssue
            | Self::ListIssueEvents => ISSUE_REFUSALS,
            Self::UpdateIssue => UPDATE_ISSUE_REFUSALS,
            Self::ManagerInspect | Self::ManagerProgress | Self::ManagerGetAction => {
                &[MANAGER_INVALID_REQUEST, MANAGER_PROJECT_NOT_IN_SCOPE]
            }
            Self::ManagerInbox | Self::ManagerReply => &[MANAGER_INVALID_REQUEST],
            Self::ManagerNotify => {
                const R: &[AgentControlRefusalV1] = &[
                    MANAGER_INVALID_REQUEST,
                    MANAGER_IDEMPOTENCY_CONFLICT,
                    refusal(
                        "manager_pending_notice_limit",
                        "at most 128 notices per Epic are unsettled; wait for the manager to read the inbox",
                    ),
                ];
                R
            }
            Self::ManagerWorkView => {
                const R: &[AgentControlRefusalV1] = &[
                    MANAGER_INVALID_REQUEST,
                    refusal(
                        "manager_work_view_work_not_live",
                        "the work_key is no longer live; read AgentManagerWorkView with {} for the live work",
                    ),
                ];
                R
            }
            Self::ManagerSend => {
                const R: &[AgentControlRefusalV1] = &[
                    MANAGER_INVALID_REQUEST,
                    MANAGER_IDEMPOTENCY_CONFLICT,
                    MANAGER_CAPABILITY_DENIED,
                ];
                R
            }
            Self::ManagerUpdate => {
                const R: &[AgentControlRefusalV1] = &[
                    MANAGER_INVALID_REQUEST,
                    MANAGER_IDEMPOTENCY_CONFLICT,
                    MANAGER_CAPABILITY_DENIED,
                    MANAGER_PROJECT_NOT_IN_SCOPE,
                    refusal(
                        "manager_review_work_version_changed",
                        "request_review expected_row_version is the work row's current row_version; the refusal names it (current_row_version=N), or read AgentManagerInspect with section work",
                    ),
                ];
                R
            }
            Self::ManagerPrepareControl | Self::ManagerCommitPreparedControl => {
                const R: &[AgentControlRefusalV1] = &[
                    MANAGER_INVALID_REQUEST,
                    MANAGER_IDEMPOTENCY_CONFLICT,
                    MANAGER_CAPABILITY_DENIED,
                    MANAGER_PROJECT_NOT_IN_SCOPE,
                ];
                R
            }
            Self::ManagerControl => {
                const R: &[AgentControlRefusalV1] = &[
                    MANAGER_INVALID_REQUEST,
                    MANAGER_IDEMPOTENCY_CONFLICT,
                    MANAGER_CAPABILITY_DENIED,
                    MANAGER_PAUSED,
                    MANAGER_HUMAN_OR_RECOVERY_OWNER,
                    MANAGER_PROJECT_NOT_IN_SCOPE,
                    refusal(
                        crate::global_manager::MANAGER_TARGET_OWNED_BY_ANCESTOR,
                        "a global manager owns that session; ask it, do not retry",
                    ),
                    refusal(
                        crate::global_manager::MANAGER_ANCESTOR_ALLOWANCE_EXCEEDED,
                        "the covering global grant's creation budget for this project is spent; ask the global manager",
                    ),
                    refusal(
                        "manager_v2_daemon_setting_not_allowlisted",
                        "ProposeDaemonSetting changes only the curated settings; spend and credentials are never adjustable",
                    ),
                    refusal(
                        "manager_v2_daemon_setting_not_adjustable",
                        "the operator set no bound for this key in the manager policy; ask the operator",
                    ),
                    refusal(
                        "manager_v2_daemon_setting_out_of_bounds",
                        "propose a value inside the operator's per-key min and max",
                    ),
                    refusal(
                        "manager_sandbox_source_invalid",
                        "sandbox_source is {\"rolling\":{}}, {\"commit\":\"<40 lowercase hex>\"} or {\"path\":\"<absolute path>\"}; nothing was changed",
                    ),
                    refusal(
                        "manager_sandbox_source_not_worktree",
                        "the path is not a registered worktree of the project repository (git worktree list); name your sandbox_root itself",
                    ),
                    refusal(
                        "manager_sandbox_source_commit_unknown",
                        "that commit is not in the project repository; commit and name an existing SHA",
                    ),
                ];
                R
            }
            Self::ReadSessionEvents => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        AGENT_READ_EVENTS_SCOPE_DENIED,
                        "read only your own session and lineage, sessions you control, or sessions in your manager scope",
                    ),
                    refusal(
                        "agent_read_events_invalid_limit",
                        "use a limit inside the schema bounds",
                    ),
                    refusal(
                        "agent_read_events_invalid_max_bytes",
                        "use a max_bytes inside the schema bounds",
                    ),
                ];
                R
            }
            Self::GetProviderStatus => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        "provider_status_not_authorized",
                        "only the appointed manager or the current lead of your Epic may read it",
                    ),
                    refusal(
                        PROVIDER_STATUS_UNKNOWN_PROVIDER,
                        "name a provider from the schema enum, or omit provider for all",
                    ),
                ];
                R
            }
            Self::SubmitJob => JOB_REFUSALS,
            Self::GetJob => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    JOB_NOT_FOUND,
                    "read only job ids you submitted; list yours with AgentListJobs",
                )];
                R
            }
            Self::CancelJob => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    JOB_NOT_FOUND,
                    "cancel only job ids you submitted; list yours with AgentListJobs",
                )];
                R
            }
            Self::ReportToHub => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        SATELLITE_REPORT_NOT_AUTHORIZED,
                        "every authorization failure looks the same: only the appointed manager that is the operator-declared seat (or its rotation tip) may report, and the operator must allowlist a hub",
                    ),
                    refusal(
                        SATELLITE_REPORT_INVALID,
                        "send a short non-empty line within the size bound without control characters",
                    ),
                    refusal(
                        SATELLITE_REPORT_QUEUE_FULL,
                        "wait for the hub to collect queued reports before sending more",
                    ),
                ];
                R
            }
            Self::SendSatelliteMessage => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        SATELLITE_TARGET_NOT_AUTHORIZED,
                        "every authorization failure looks the same: check the operator enabled dispatch and declared this peer and session in your scope",
                    ),
                    refusal(
                        SATELLITE_MESSAGE_INVALID,
                        "send a non-empty message within the size bound",
                    ),
                    refusal(
                        SATELLITE_MESSAGE_KEY_INVALID,
                        "use an idempotency_key of at most 128 bytes without NUL",
                    ),
                    refusal(
                        SATELLITE_MESSAGE_QUEUE_FULL,
                        "wait for queued satellite messages to be delivered before sending more",
                    ),
                    refusal(
                        SATELLITE_MESSAGE_KEY_CONFLICT,
                        "retry with the original request or use a new idempotency_key",
                    ),
                ];
                R
            }
            Self::EnqueueLandingSource => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        QUEUE_DISABLED,
                        "the operator has not enabled the rolling queue; land with rsi-rolling-land instead",
                    ),
                    refusal(
                        QUEUE_SOURCE_INVALID,
                        "enqueue a full 40-hex commit reachable from your sandbox",
                    ),
                    refusal(
                        QUEUE_FILTER_INVALID,
                        "use PACKAGE=FILTER test_filters within the schema bounds",
                    ),
                    refusal(
                        QUEUE_FILTER_MATCHES_NO_TESTS,
                        "a test filter selects no test; the refusal names it: fix the filter (shard and test name) and enqueue again",
                    ),
                    refusal(
                        QUEUE_KEY_INVALID,
                        "use an idempotency_key of at most 128 bytes without NUL",
                    ),
                    refusal(
                        QUEUE_DUPLICATE_SOURCE,
                        "this commit is already queued; wait for its outcome",
                    ),
                    refusal(
                        QUEUE_NOT_AUTHORIZED,
                        "only the appointed manager or an Epic lead may enqueue a landing source",
                    ),
                    refusal(
                        "queue_idempotency_conflict",
                        "retry with the original request or use a new idempotency_key",
                    ),
                    refusal(
                        QUEUE_REGATE_EXHAUSTED,
                        "an out-of-band push to rolling cost a second regate; merge the new tip and enqueue again",
                    ),
                    refusal(
                        QUEUE_GATE_TIMEOUT,
                        "the batch ran past the operator's gate wall-time budget (rolling_queue_gate_timeout_mins); nothing was published: enqueue again, with narrower test filters if you can",
                    ),
                ];
                R
            }
            Self::RequestDeploy => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        DEPLOY_NOT_AUTHORIZED,
                        "only the current appointed manager may request a deploy",
                    ),
                    refusal(
                        DEPLOY_CAPABILITY_REQUIRED,
                        "the operator must grant the Deploy capability first",
                    ),
                    refusal(
                        DEPLOY_EXECUTE_REQUIRED,
                        "deploy needs Execute mode and an unpaused seat",
                    ),
                    refusal(
                        DEPLOY_SHA_INVALID,
                        "send the full 40-hex sha of the built commit",
                    ),
                    refusal(
                        DEPLOY_KEY_CONFLICT,
                        "retry with the original request or use a new idempotency_key",
                    ),
                    refusal(
                        DEPLOY_IN_PROGRESS,
                        "wait for the running deploy's wake before requesting another",
                    ),
                    refusal(
                        DEPLOY_SHA_MISMATCH,
                        "rebuild so the staged binaries embed the requested sha, then retry",
                    ),
                    refusal(
                        DEPLOY_NEEDS_SUPERVISOR,
                        "the daemon is not running under rsid-supervisor.sh; ask the operator",
                    ),
                    refusal(
                        DEPLOY_TARGET_MISMATCH,
                        "the supervisor runs a different rsid than a deploy installs; ask the operator to move it (make release-install NOW=1)",
                    ),
                    refusal(
                        "deploy_build_not_supported",
                        "build first (install-release.sh --no-restart) and deploy the built binaries_dir",
                    ),
                    refusal(
                        "deploy_schema_downgrade",
                        "the staged rsid has a schema below the live one; build from a newer commit",
                    ),
                    refusal(
                        "deploy_restart_budget",
                        "at most two deploy restarts per hour; wait before requesting another",
                    ),
                    refusal(
                        "deploy_staged_copy_changed",
                        "a staged binary changed before the swap and the deploy rolled back; restage and retry",
                    ),
                    refusal(
                        SATELLITE_TARGET_NOT_AUTHORIZED,
                        "with peer_id every authorization failure looks the same: check pairing, dispatch and declared scope",
                    ),
                    refusal(
                        "satellite_deploy_owner_required",
                        "the satellite needs a live scope root as the deploy owner",
                    ),
                    refusal(
                        "satellite_deploy_unreachable",
                        "no link to the satellite verified; check its daemon and retry",
                    ),
                ];
                R
            }
            Self::TopologyUpsert => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        "topology_invalid_idempotency_key",
                        "use an idempotency_key of at most 128 bytes without NUL",
                    ),
                    refusal(
                        "idempotency_conflict",
                        "the key binds its first request; use a new key for a revised definition",
                    ),
                    refusal(
                        "stale_revision",
                        "re-list the topology and retry with its current expected_revision",
                    ),
                    refusal(
                        "name_conflict",
                        "topology names are unique per owner; choose another name",
                    ),
                ];
                R
            }
            Self::TopologyExecute => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        "idempotency_conflict",
                        "the key binds its first request; use a new key for a changed request",
                    ),
                    refusal(
                        "topology_not_visible",
                        "the topology is not shared or visible to you; list the topologies in your scope",
                    ),
                    refusal(
                        "topology_changed",
                        "the topology was revised; re-list it and execute with the new definition_digest",
                    ),
                ];
                R
            }
            Self::TopologyResolveAttempt => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    "discard_requires_manager",
                    "a lead may inspect, accept or retry in its own Epic; discard needs the manager",
                )];
                R
            }
            Self::TopologyInterrupt => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    "topology_invalid_expected_row_version",
                    "refresh with AgentTopologyGetExecution and retry with its row_version",
                )];
                R
            }
            Self::TopologyList | Self::TopologyGetExecution => &[],
            Self::CancelWake => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        "wake_not_found",
                        "cancel only your own wakes; list them with AgentListWakes",
                    ),
                    refusal(
                        "wake_protected",
                        "program guards and manager watches are daemon-owned and cannot be cancelled",
                    ),
                ];
                R
            }
            Self::Halt => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    "agent_verb_scope_denied",
                    "halt yourself, a direct child or a child of an Epic you lead; a manager cannot halt an Epic lead",
                )];
                R
            }
            Self::QueryFailureSignatures => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    "failure_signature_query_invalid",
                    "send test_id, digest (64 lowercase hex) or both; at least one is required",
                )];
                R
            }
            Self::ManagerLaunchIssueWorker => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        "manager_issue_worker_invalid_request",
                        "send issue (display number >= 1), a non-empty brief, launch and an idempotency_key",
                    ),
                    refusal(
                        "manager_issue_worker_issue_unavailable",
                        "the Issue is not in your project, is archived, or is Closed or Cancelled; nothing was changed",
                    ),
                    refusal(
                        "manager_issue_worker_parent_required",
                        "name parent_epic_id: your scope holds no single Epic to default to",
                    ),
                    refusal(
                        "manager_issue_worker_authority_denied",
                        "needs the IssueCoordinate grant as well as SessionCreate; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        crate::global_manager::MANAGER_ISSUE_WORKER_ALREADY_LIVE,
                        "another manager's worker on this Issue is still live; watch it instead of launching a second",
                    ),
                    MANAGER_PROJECT_NOT_IN_SCOPE,
                    refusal(
                        "manager_sandbox_source_invalid",
                        "sandbox_source is {\"rolling\":{}}, {\"commit\":\"<40 lowercase hex>\"} or {\"path\":\"<absolute path>\"}; nothing was changed",
                    ),
                    refusal(
                        "manager_sandbox_source_not_worktree",
                        "the path is not a registered worktree of the project repository (git worktree list); name your sandbox_root itself",
                    ),
                    refusal(
                        "manager_sandbox_source_commit_unknown",
                        "that commit is not in the project repository; commit and name an existing SHA",
                    ),
                    refusal(
                        crate::manager_issue_worker::MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE,
                        "continue_from names a worker that is still running; wait for its turn to end (your terminal watch fires) or halt it first",
                    ),
                    refusal(
                        crate::manager_issue_worker::MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE,
                        "continue_from names a worker bound to a different Issue; launch for that Issue or omit continue_from",
                    ),
                    refusal(
                        crate::manager_issue_worker::MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE,
                        "review_of names no Issue of this project; pass the implementer Issue's display number, or omit review_of",
                    ),
                    refusal(
                        crate::manager_issue_worker::MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE,
                        "continue_from names no Issue-bound worker of this project; name the session id from your worker_context_cap notice",
                    ),
                    refusal(
                        crate::manager_issue_worker::MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE,
                        "the predecessor's sandbox is gone; launch with sandbox_source {\"commit\": \"<its last SHA>\"} instead",
                    ),
                ];
                R
            }
            Self::GlobalOverview => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    GLOBAL_MANAGER_NOT_SEAT,
                    "only the operator-appointed global seat may call it; re-check AgentGetAuthorityCatalog",
                )];
                R
            }
            Self::ManagerOverview => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    MANAGER_TIER_NOT_NODE_SEAT,
                    "only a manager seat (area, project or portfolio node) has a node to read; re-check AgentGetAuthorityCatalog",
                )];
                R
            }
            Self::GlobalSend => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        GLOBAL_MANAGER_NOT_SEAT,
                        "only the operator-appointed global seat may call it; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        GLOBAL_PROJECT_NOT_IN_GRANT,
                        "name a project from AgentGlobalOverview; the operator owns the project list",
                    ),
                    refusal(
                        GLOBAL_PROJECT_HAS_NO_MANAGER,
                        "appoint a project manager with AgentGlobalAppointManager first",
                    ),
                    refusal(
                        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT,
                        "replay the original content or use a new idempotency_key",
                    ),
                    refusal(
                        GLOBAL_MANAGER_MAILBOX_FULL,
                        "wait for the recipient to take its queued messages",
                    ),
                ];
                R
            }
            Self::GlobalAppointManager => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        GLOBAL_MANAGER_NOT_SEAT,
                        "only the operator-appointed global seat may call it; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        GLOBAL_PROJECT_NOT_IN_GRANT,
                        "name a project from AgentGlobalOverview; the operator owns the project list",
                    ),
                    refusal(
                        GLOBAL_LAUNCH_NOT_ALLOWED,
                        "pick a launch from your grant's allowed_launches",
                    ),
                    refusal(
                        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT,
                        "replay the original content or use a new idempotency_key",
                    ),
                    refusal(
                        "manager_node_root_has_active_delegates",
                        "the current project manager has active area delegates; ask it to revoke them first",
                    ),
                    refusal(
                        MANAGER_DIRECT_REPORT_CAP,
                        "you already have max_direct_reports children and project managers; revoke one or ask the operator",
                    ),
                ];
                R
            }
            Self::ManagerAppointChild => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        GLOBAL_MANAGER_NOT_SEAT,
                        "only an active portfolio node seat may call it; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        crate::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE,
                        "name projects of your own coverage; a child's coverage is a strict subset of yours",
                    ),
                    refusal(
                        GLOBAL_LAUNCH_NOT_ALLOWED,
                        "pick a launch from your grant's allowed_launches",
                    ),
                    refusal(
                        MANAGER_SCOPE_NOT_NARROWED,
                        "a child covers a strict subset of your projects; act in the rest yourself",
                    ),
                    refusal(
                        MANAGER_SCOPE_OVERLAP,
                        "another child already covers one of these projects; narrow the set or revoke that child",
                    ),
                    refusal(
                        MANAGER_DIRECT_REPORT_CAP,
                        "you already have max_direct_reports children and project managers; revoke one or ask the operator",
                    ),
                    refusal(
                        MANAGER_CAPABILITY_WIDENED,
                        "give the child only capabilities and launches you hold",
                    ),
                    refusal(
                        MANAGER_ALLOWANCE_EXCEEDED,
                        "set every finite allowance strictly below yours and max_direct_reports at most yours",
                    ),
                    refusal(
                        MANAGER_CHILD_OPERATOR_GRANTED,
                        "the operator granted that child; only the operator re-seats it",
                    ),
                    refusal(
                        MANAGER_NODE_NOT_IN_SCOPE,
                        "name a child node your node granted",
                    ),
                    refusal(
                        MANAGER_NODE_ROOT_OPERATOR_ONLY,
                        "only the operator creates roots, adopts or re-parents",
                    ),
                    refusal(
                        MANAGER_NODE_STALE,
                        "re-read the child's grant version and retry",
                    ),
                    refusal(
                        PORTFOLIO_IDEMPOTENCY_CONFLICT,
                        "replay the original content or use a new idempotency_key",
                    ),
                    refusal(
                        "manager_node_root_has_active_delegates",
                        "the current project manager has active area delegates; ask it to revoke them first",
                    ),
                ];
                R
            }
            Self::ManagerRevokeChild => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        GLOBAL_MANAGER_NOT_SEAT,
                        "only an active portfolio node seat may call it; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        MANAGER_CHILD_OPERATOR_GRANTED,
                        "the operator granted that child; ask the operator to revoke it",
                    ),
                    refusal(
                        MANAGER_NODE_NOT_IN_SCOPE,
                        "name a child node your node granted",
                    ),
                    refusal(
                        MANAGER_NODE_STALE,
                        "re-read the child's grant version and retry",
                    ),
                ];
                R
            }
            Self::ReportToGlobal => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        GLOBAL_REPORT_NOT_AUTHORIZED,
                        "only the current project manager of a project in the global grant may report up",
                    ),
                    refusal(
                        GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT,
                        "replay the original content or use a new idempotency_key",
                    ),
                    refusal(
                        GLOBAL_MANAGER_MAILBOX_FULL,
                        "wait for the global manager to take its queued messages",
                    ),
                ];
                R
            }
            Self::ReportUp => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        MANAGER_TIER_NOT_NODE_SEAT,
                        "only a manager seat (area, project or portfolio node) reports up; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        MANAGER_TIER_TARGET_VACANT,
                        "the manager above you has no live seat; ask the operator to appoint one",
                    ),
                    refusal(
                        MANAGER_TIER_IDEMPOTENCY_CONFLICT,
                        "replay the original content or use a new idempotency_key",
                    ),
                    refusal(
                        MANAGER_TIER_MAILBOX_FULL,
                        "wait for the recipient to take its queued messages",
                    ),
                ];
                R
            }
            Self::SendDown => {
                const R: &[AgentControlRefusalV1] = &[
                    refusal(
                        MANAGER_TIER_NOT_NODE_SEAT,
                        "only a manager seat (area, project or portfolio node) sends down; re-check AgentGetAuthorityCatalog",
                    ),
                    refusal(
                        MANAGER_TARGET_NOT_DESCENDANT,
                        "mail goes down only: name a node below yours, never your own or an ancestor (report up with AgentReportUp)",
                    ),
                    MANAGER_PROJECT_NOT_IN_SCOPE,
                    refusal(
                        MANAGER_TIER_TARGET_UNKNOWN,
                        "name an existing node (AgentGlobalOverview lists your projects)",
                    ),
                    refusal(
                        MANAGER_TIER_TARGET_VACANT,
                        "that node has no live seat; appoint one first",
                    ),
                    refusal(
                        MANAGER_TIER_IDEMPOTENCY_CONFLICT,
                        "replay the original content or use a new idempotency_key",
                    ),
                    refusal(
                        MANAGER_TIER_MAILBOX_FULL,
                        "wait for the recipient to take its queued messages",
                    ),
                ];
                R
            }
            Self::GetDaemonInfo => {
                const R: &[AgentControlRefusalV1] = &[refusal(
                    "daemon_info_not_authorized",
                    "only the appointed manager or the current lead of your Epic may read it",
                )];
                R
            }
            Self::ReserveSuccessor
            | Self::GetStatus
            | Self::ListWakes
            | Self::SubmitReviewReceipt
            | Self::ManagerDelegateNode
            | Self::ManagerEscalate
            | Self::ManagerListEscalations
            | Self::ManagerResolveEscalation
            | Self::ListJobs => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent_control_schema::{AgentControlVerbV1, agent_control_catalog_v1};
    use std::collections::BTreeSet;

    #[test]
    fn every_verb_has_an_example_that_validates_against_its_schema() {
        for descriptor in agent_control_catalog_v1() {
            let example = descriptor.verb.example();
            assert!(
                example.is_object(),
                "{} example must be a params object",
                descriptor.method
            );
            assert_eq!(
                descriptor.verb.validate_params(&example),
                Ok(()),
                "{} example must validate",
                descriptor.method
            );
            let schema = descriptor.parameters();
            let properties = schema["properties"].as_object().unwrap();
            for key in example.as_object().unwrap().keys() {
                assert!(
                    properties.contains_key(key),
                    "{} example field {key} is absent from its schema",
                    descriptor.method
                );
            }
            for required in schema["required"].as_array().into_iter().flatten() {
                assert!(
                    example.get(required.as_str().unwrap()).is_some(),
                    "{} example omits required field {required}",
                    descriptor.method
                );
            }
        }
    }

    #[test]
    fn refusals_are_unique_stable_codes_with_a_next_action() {
        let mut without_refusals = BTreeSet::new();
        for descriptor in agent_control_catalog_v1() {
            let refusals = descriptor.verb.refusals();
            if refusals.is_empty() {
                without_refusals.insert(descriptor.method);
            }
            let mut seen = BTreeSet::new();
            for refusal in refusals {
                assert!(
                    !refusal.code.is_empty()
                        && refusal.code.bytes().all(|b| {
                            b.is_ascii_lowercase()
                                || b.is_ascii_digit()
                                || b == b'_'
                                || b == b':'
                                || b.is_ascii_uppercase()
                        }),
                    "{}: bad refusal code {:?}",
                    descriptor.method,
                    refusal.code
                );
                assert!(
                    refusal.next_action.len() > 8,
                    "{}: {} needs a next_action",
                    descriptor.method,
                    refusal.code
                );
                assert!(
                    seen.insert(refusal.code),
                    "{}: duplicate refusal {}",
                    descriptor.method,
                    refusal.code
                );
            }
        }
        // Verbs whose refusals are still free-text: pinned so a verb cannot
        // silently lose or skip its refusal list. Shrink this set, never grow it.
        assert_eq!(
            without_refusals,
            BTreeSet::from([
                "AgentGetStatus",
                "AgentListJobs",
                "AgentListWakes",
                "AgentManagerDelegateNode",
                "AgentManagerEscalate",
                "AgentManagerListEscalations",
                "AgentManagerResolveEscalation",
                "AgentReserveSuccessor",
                "AgentSubmitReviewReceipt",
                "AgentTopologyGetExecution",
                "AgentTopologyList",
            ])
        );
    }

    #[test]
    fn typed_refusal_families_match_their_wire_codes() {
        let codes =
            |verb: AgentControlVerbV1| verb.refusals().iter().map(|r| r.code).collect::<Vec<_>>();
        assert!(codes(AgentControlVerbV1::ContinueChild).contains(&"agent_continue_stale_cursor"));
        assert!(codes(AgentControlVerbV1::ArchiveChild).contains(&"agent_archive_stale_cursor"));
        assert!(codes(AgentControlVerbV1::SendMessage).contains(&"agent_message_target_terminal"));
        assert!(codes(AgentControlVerbV1::UpdateIssue).contains(&"stale_version"));
        assert!(codes(AgentControlVerbV1::SubmitJob).contains(&"job_kind_not_authorized"));
        assert!(
            codes(AgentControlVerbV1::SubmitJob)
                .contains(&crate::agent_jobs::JOB_PLATFORM_UNSUPPORTED)
        );
    }
}
