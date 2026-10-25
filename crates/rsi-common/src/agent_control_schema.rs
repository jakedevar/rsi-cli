//! Closed, versioned request-schema catalog for the agent-control surface.
//!
//! This module describes only the supported JSON request shape of the closed
//! attributed `Agent*` RPC verbs. It is not an authorization registry and it
//! does not replace DTO deserialization or runtime validation. In particular,
//! topology, lead scope, UUID non-nilness, provider/model availability, Issue
//! state transitions, content-patch semantics, and wake timing combinations
//! remain daemon-enforced predicates.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;
use uuid::Uuid;
mod diagnostics;
mod manager_v2;
#[cfg(test)]
mod parameter_names;

/// Version of the deterministic schema-discovery envelope.
pub const AGENT_CONTROL_SCHEMA_VERSION_V1: u32 = 1;

/// #2313: request parameters that were renamed to the one canonical name for
/// their concept. Each entry is `(verb, deprecated alias, canonical name)`.
/// The DTO still accepts the alias for one release (serde `alias`), and the
/// verb's schema lists it with `"deprecated": true` next to the canonical
/// property. The `parameter_names` schema test fails on any other
/// non-canonical name, so a new alias must be registered here on purpose.
pub const DEPRECATED_PARAMETER_ALIASES: &[(AgentControlVerbV1, &str, &str)] = &[
    (
        AgentControlVerbV1::SendMessage,
        "target_session_id",
        "session_id",
    ),
    (
        AgentControlVerbV1::ContinueChild,
        "target_session_id",
        "session_id",
    ),
    (
        AgentControlVerbV1::ArchiveChild,
        "target_session_id",
        "session_id",
    ),
    (
        AgentControlVerbV1::SubmitJob,
        "sandbox_session_id",
        "source_session_id",
    ),
];

/// The closed v1 agent-control request catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentControlVerbV1 {
    GetAuthorityCatalog,
    SpawnChild,
    ReserveSuccessor,
    GetProgress,
    SendMessage,
    GetStatus,
    Halt,
    ContinueChild,
    ArchiveChild,
    ScheduleWake,
    CancelWake,
    ListWakes,
    CreateIssue,
    ListIssues,
    GetIssue,
    UpdateIssue,
    UpdateIssueStatus,
    ArchiveIssue,
    RestoreIssue,
    ListIssueEvents,
    ManagerProgress,
    ManagerInbox,
    ManagerSend,
    ManagerReply,
    ManagerNotify,
    ManagerInspect,
    ManagerUpdate,
    SubmitReviewReceipt,
    ManagerControl,
    ManagerPrepareControl,
    ManagerCommitPreparedControl,
    ManagerGetAction,
    ManagerLaunchIssueWorker,
    ManagerWorkView,
    ManagerDelegateNode,
    ManagerEscalate,
    ManagerListEscalations,
    ManagerResolveEscalation,
    TopologyUpsert,
    TopologyList,
    TopologyGet,
    TopologyExecute,
    TopologyGetExecution,
    TopologyInterrupt,
    TopologyResolveAttempt,
    EnqueueLandingSource,
    ListRollingQueue,
    CancelRollingQueueEntry,
    CheckLandingCandidate,
    ReadSessionEvents,
    GetProviderStatus,
    SubmitJob,
    GetJob,
    ListJobs,
    CancelJob,
    SendSatelliteMessage,
    ReportToHub,
    GetDaemonInfo,
    RequestDeploy,
    SwitchHostPreset,
    PushSatelliteManagerPolicy,
    ReseatSatelliteManager,
    QueryFailureSignatures,
    WorkerRoster,
    GlobalOverview,
    GlobalSend,
    GlobalAppointManager,
    ReportToGlobal,
    ReportUp,
    SendDown,
    ManagerAppointChild,
    ManagerRevokeChild,
    ManagerOverview,
    CreateProject,
    UpdateProject,
    OperatorTodoUpsert,
    OperatorTodoClose,
    StandingDirective,
    ListSourceWorktreeCohorts,
    AuditSourceWorktreeCohort,
    ApplySourceWorktreeCohort,
}

/// Native tool whose advertised input is the same request schema as one
/// catalog verb. Registration and authority remain transport-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeAgentControlToolV1 {
    RsiControlAuthorityCatalog,
    RsiControlSpawn,
    RsiControlReserveSuccessor,
    RsiControlProgress,
    RsiControlSendMessage,
    RsiControlStatus,
    RsiControlHalt,
    ScheduleWake,
    RsiControlCreateIssue,
    RsiControlListIssues,
    RsiControlGetIssue,
    RsiControlUpdateIssue,
    RsiControlUpdateIssueStatus,
    RsiControlArchiveIssue,
    RsiControlRestoreIssue,
    RsiControlListIssueEvents,
    RsiControlManagerProgress,
    RsiControlManagerInbox,
    RsiControlManagerSend,
    RsiControlManagerReply,
    RsiControlManagerNotify,
    RsiControlManagerInspect,
    RsiControlManagerUpdate,
    RsiControlSubmitReviewReceipt,
    RsiControlManagerControl,
    RsiControlManagerPrepareControl,
    RsiControlManagerCommitPreparedControl,
    RsiControlManagerGetAction,
    RsiControlManagerLaunchIssueWorker,
    RsiControlManagerWorkView,
    RsiControlTopologyUpsert,
    RsiControlTopologyList,
    RsiControlTopologyGet,
    RsiControlTopologyExecute,
    RsiControlTopologyGetExecution,
    RsiControlTopologyInterrupt,
    RsiControlTopologyResolveAttempt,
    RsiControlReadSessionEvents,
    RsiControlQueryFailureSignatures,
    RsiControlWorkerRoster,
    RsiControlListWakes,
    RsiControlGlobalOverview,
    RsiControlGlobalSend,
    RsiControlReportToGlobal,
    RsiControlReportUp,
    RsiControlSendDown,
    RsiControlManagerOverview,
    RsiControlCreateProject,
    RsiControlUpdateProject,
    RsiControlOperatorTodoUpsert,
    RsiControlOperatorTodoClose,
    RsiControlStandingDirective,
    RsiControlListSourceWorktreeCohorts,
    RsiControlAuditSourceWorktreeCohort,
    RsiControlApplySourceWorktreeCohort,
    RsiControlListRollingQueue,
    RsiControlCancelRollingQueueEntry,
    RsiControlCheckLandingCandidate,
    RsiControlContinueChild,
    RsiControlCancelWake,
    RsiControlManagerDelegateNode,
    RsiControlManagerEscalate,
    RsiControlManagerListEscalations,
    RsiControlManagerResolveEscalation,
    RsiControlEnqueueLandingSource,
    RsiControlGetProviderStatus,
    RsiControlSubmitJob,
    RsiControlGetJob,
    RsiControlListJobs,
    RsiControlCancelJob,
    RsiControlSendSatelliteMessage,
    RsiControlGetDaemonInfo,
    RsiControlRequestDeploy,
}

impl NativeAgentControlToolV1 {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RsiControlAuthorityCatalog => "rsi_control_authority_catalog",
            Self::RsiControlSpawn => "rsi_control_spawn",
            Self::RsiControlReserveSuccessor => "rsi_control_reserve_successor",
            Self::RsiControlProgress => "rsi_control_progress",
            Self::RsiControlSendMessage => "rsi_control_send_message",
            Self::RsiControlStatus => "rsi_control_status",
            Self::RsiControlHalt => "rsi_control_halt",
            Self::ScheduleWake => "schedule_wake",
            Self::RsiControlCreateIssue => "rsi_control_create_issue",
            Self::RsiControlListIssues => "rsi_control_list_issues",
            Self::RsiControlGetIssue => "rsi_control_get_issue",
            Self::RsiControlUpdateIssue => "rsi_control_update_issue",
            Self::RsiControlUpdateIssueStatus => "rsi_control_update_issue_status",
            Self::RsiControlArchiveIssue => "rsi_control_archive_issue",
            Self::RsiControlRestoreIssue => "rsi_control_restore_issue",
            Self::RsiControlListIssueEvents => "rsi_control_list_issue_events",
            Self::RsiControlManagerProgress => "rsi_control_manager_progress",
            Self::RsiControlManagerInbox => "rsi_control_manager_inbox",
            Self::RsiControlManagerSend => "rsi_control_manager_send",
            Self::RsiControlManagerReply => "rsi_control_manager_reply",
            Self::RsiControlManagerNotify => "rsi_control_manager_notify",
            Self::RsiControlManagerInspect => "rsi_control_manager_inspect",
            Self::RsiControlManagerUpdate => "rsi_control_manager_update",
            Self::RsiControlSubmitReviewReceipt => "rsi_control_submit_review_receipt",
            Self::RsiControlManagerControl => "rsi_control_manager_control",
            Self::RsiControlManagerPrepareControl => "rsi_control_manager_prepare_control",
            Self::RsiControlManagerCommitPreparedControl => {
                "rsi_control_manager_commit_prepared_control"
            }
            Self::RsiControlManagerGetAction => "rsi_control_manager_get_action",
            Self::RsiControlManagerLaunchIssueWorker => "rsi_control_manager_launch_issue_worker",
            Self::RsiControlManagerWorkView => "rsi_control_manager_work_view",
            Self::RsiControlTopologyUpsert => "rsi_control_topology_upsert",
            Self::RsiControlTopologyList => "rsi_control_topology_list",
            Self::RsiControlTopologyGet => "rsi_control_topology_get",
            Self::RsiControlTopologyExecute => "rsi_control_topology_execute",
            Self::RsiControlTopologyGetExecution => "rsi_control_topology_get_execution",
            Self::RsiControlTopologyInterrupt => "rsi_control_topology_interrupt",
            Self::RsiControlTopologyResolveAttempt => "rsi_control_topology_resolve_attempt",
            Self::RsiControlReadSessionEvents => "rsi_control_read_session_events",
            Self::RsiControlQueryFailureSignatures => "rsi_control_query_failure_signatures",
            Self::RsiControlWorkerRoster => "rsi_control_worker_roster",
            Self::RsiControlListWakes => "rsi_control_list_wakes",
            Self::RsiControlGlobalOverview => "rsi_control_global_overview",
            Self::RsiControlGlobalSend => "rsi_control_global_send",
            Self::RsiControlReportToGlobal => "rsi_control_report_to_global",
            Self::RsiControlReportUp => "rsi_control_report_up",
            Self::RsiControlSendDown => "rsi_control_send_down",
            Self::RsiControlManagerOverview => "rsi_control_manager_overview",
            Self::RsiControlCreateProject => "rsi_control_create_project",
            Self::RsiControlUpdateProject => "rsi_control_update_project",
            Self::RsiControlOperatorTodoUpsert => "rsi_control_operator_todo_upsert",
            Self::RsiControlOperatorTodoClose => "rsi_control_operator_todo_close",
            Self::RsiControlStandingDirective => "rsi_control_standing_directive",
            Self::RsiControlListSourceWorktreeCohorts => "rsi_control_list_source_worktree_cohorts",
            Self::RsiControlAuditSourceWorktreeCohort => "rsi_control_audit_source_worktree_cohort",
            Self::RsiControlApplySourceWorktreeCohort => "rsi_control_apply_source_worktree_cohort",
            Self::RsiControlListRollingQueue => "rsi_control_list_rolling_queue",
            Self::RsiControlCancelRollingQueueEntry => "rsi_control_cancel_rolling_queue_entry",
            Self::RsiControlCheckLandingCandidate => "rsi_control_check_landing_candidate",
            Self::RsiControlContinueChild => "rsi_control_continue_child",
            Self::RsiControlCancelWake => "rsi_control_cancel_wake",
            Self::RsiControlManagerDelegateNode => "rsi_control_manager_delegate_node",
            Self::RsiControlManagerEscalate => "rsi_control_manager_escalate",
            Self::RsiControlManagerListEscalations => "rsi_control_manager_list_escalations",
            Self::RsiControlManagerResolveEscalation => "rsi_control_manager_resolve_escalation",
            Self::RsiControlEnqueueLandingSource => "rsi_control_enqueue_landing_source",
            Self::RsiControlGetProviderStatus => "rsi_control_get_provider_status",
            Self::RsiControlSubmitJob => "rsi_control_submit_job",
            Self::RsiControlGetJob => "rsi_control_get_job",
            Self::RsiControlListJobs => "rsi_control_list_jobs",
            Self::RsiControlCancelJob => "rsi_control_cancel_job",
            Self::RsiControlSendSatelliteMessage => "rsi_control_send_satellite_message",
            Self::RsiControlGetDaemonInfo => "rsi_control_get_daemon_info",
            Self::RsiControlRequestDeploy => "rsi_control_request_deploy",
        }
    }

    /// #3774: tools the `rsi-agent-mcp` gateway serves by forwarding the verb to
    /// the daemon socket. Their handlers live in the RPC layer (jobs, deploy,
    /// queue, daemon info, ...), which the in-process Harness and
    /// CodexAppServer registries do not reach, so those registries do not
    /// register them; the verb and its authority checks are unchanged.
    #[must_use]
    pub const fn mcp_gateway_only(self) -> bool {
        matches!(
            self,
            Self::RsiControlContinueChild
                | Self::RsiControlCancelWake
                | Self::RsiControlManagerDelegateNode
                | Self::RsiControlManagerEscalate
                | Self::RsiControlManagerListEscalations
                | Self::RsiControlManagerResolveEscalation
                | Self::RsiControlEnqueueLandingSource
                | Self::RsiControlGetProviderStatus
                | Self::RsiControlSubmitJob
                | Self::RsiControlGetJob
                | Self::RsiControlListJobs
                | Self::RsiControlCancelJob
                | Self::RsiControlSendSatelliteMessage
                | Self::RsiControlGetDaemonInfo
                | Self::RsiControlRequestDeploy
        )
    }

    /// The closed RPC verb served by this native tool.
    #[must_use]
    pub const fn verb(self) -> AgentControlVerbV1 {
        match self {
            Self::RsiControlAuthorityCatalog => AgentControlVerbV1::GetAuthorityCatalog,
            Self::RsiControlSpawn => AgentControlVerbV1::SpawnChild,
            Self::RsiControlReserveSuccessor => AgentControlVerbV1::ReserveSuccessor,
            Self::RsiControlProgress => AgentControlVerbV1::GetProgress,
            Self::RsiControlSendMessage => AgentControlVerbV1::SendMessage,
            Self::RsiControlStatus => AgentControlVerbV1::GetStatus,
            Self::RsiControlHalt => AgentControlVerbV1::Halt,
            Self::ScheduleWake => AgentControlVerbV1::ScheduleWake,
            Self::RsiControlCreateIssue => AgentControlVerbV1::CreateIssue,
            Self::RsiControlListIssues => AgentControlVerbV1::ListIssues,
            Self::RsiControlGetIssue => AgentControlVerbV1::GetIssue,
            Self::RsiControlUpdateIssue => AgentControlVerbV1::UpdateIssue,
            Self::RsiControlUpdateIssueStatus => AgentControlVerbV1::UpdateIssueStatus,
            Self::RsiControlArchiveIssue => AgentControlVerbV1::ArchiveIssue,
            Self::RsiControlRestoreIssue => AgentControlVerbV1::RestoreIssue,
            Self::RsiControlListIssueEvents => AgentControlVerbV1::ListIssueEvents,
            Self::RsiControlManagerProgress => AgentControlVerbV1::ManagerProgress,
            Self::RsiControlManagerInbox => AgentControlVerbV1::ManagerInbox,
            Self::RsiControlManagerSend => AgentControlVerbV1::ManagerSend,
            Self::RsiControlManagerReply => AgentControlVerbV1::ManagerReply,
            Self::RsiControlManagerNotify => AgentControlVerbV1::ManagerNotify,
            Self::RsiControlManagerInspect => AgentControlVerbV1::ManagerInspect,
            Self::RsiControlManagerUpdate => AgentControlVerbV1::ManagerUpdate,
            Self::RsiControlSubmitReviewReceipt => AgentControlVerbV1::SubmitReviewReceipt,
            Self::RsiControlManagerControl => AgentControlVerbV1::ManagerControl,
            Self::RsiControlManagerPrepareControl => AgentControlVerbV1::ManagerPrepareControl,
            Self::RsiControlManagerCommitPreparedControl => {
                AgentControlVerbV1::ManagerCommitPreparedControl
            }
            Self::RsiControlManagerGetAction => AgentControlVerbV1::ManagerGetAction,
            Self::RsiControlManagerLaunchIssueWorker => {
                AgentControlVerbV1::ManagerLaunchIssueWorker
            }
            Self::RsiControlManagerWorkView => AgentControlVerbV1::ManagerWorkView,
            Self::RsiControlTopologyUpsert => AgentControlVerbV1::TopologyUpsert,
            Self::RsiControlTopologyList => AgentControlVerbV1::TopologyList,
            Self::RsiControlTopologyGet => AgentControlVerbV1::TopologyGet,
            Self::RsiControlTopologyExecute => AgentControlVerbV1::TopologyExecute,
            Self::RsiControlTopologyGetExecution => AgentControlVerbV1::TopologyGetExecution,
            Self::RsiControlTopologyInterrupt => AgentControlVerbV1::TopologyInterrupt,
            Self::RsiControlTopologyResolveAttempt => AgentControlVerbV1::TopologyResolveAttempt,
            Self::RsiControlReadSessionEvents => AgentControlVerbV1::ReadSessionEvents,
            Self::RsiControlQueryFailureSignatures => AgentControlVerbV1::QueryFailureSignatures,
            Self::RsiControlWorkerRoster => AgentControlVerbV1::WorkerRoster,
            Self::RsiControlListWakes => AgentControlVerbV1::ListWakes,
            Self::RsiControlGlobalOverview => AgentControlVerbV1::GlobalOverview,
            Self::RsiControlGlobalSend => AgentControlVerbV1::GlobalSend,
            Self::RsiControlReportToGlobal => AgentControlVerbV1::ReportToGlobal,
            Self::RsiControlReportUp => AgentControlVerbV1::ReportUp,
            Self::RsiControlSendDown => AgentControlVerbV1::SendDown,
            Self::RsiControlManagerOverview => AgentControlVerbV1::ManagerOverview,
            Self::RsiControlCreateProject => AgentControlVerbV1::CreateProject,
            Self::RsiControlUpdateProject => AgentControlVerbV1::UpdateProject,
            Self::RsiControlOperatorTodoUpsert => AgentControlVerbV1::OperatorTodoUpsert,
            Self::RsiControlOperatorTodoClose => AgentControlVerbV1::OperatorTodoClose,
            Self::RsiControlStandingDirective => AgentControlVerbV1::StandingDirective,
            Self::RsiControlListSourceWorktreeCohorts => {
                AgentControlVerbV1::ListSourceWorktreeCohorts
            }
            Self::RsiControlAuditSourceWorktreeCohort => {
                AgentControlVerbV1::AuditSourceWorktreeCohort
            }
            Self::RsiControlApplySourceWorktreeCohort => {
                AgentControlVerbV1::ApplySourceWorktreeCohort
            }
            Self::RsiControlListRollingQueue => AgentControlVerbV1::ListRollingQueue,
            Self::RsiControlCancelRollingQueueEntry => AgentControlVerbV1::CancelRollingQueueEntry,
            Self::RsiControlCheckLandingCandidate => AgentControlVerbV1::CheckLandingCandidate,
            Self::RsiControlContinueChild => AgentControlVerbV1::ContinueChild,
            Self::RsiControlCancelWake => AgentControlVerbV1::CancelWake,
            Self::RsiControlManagerDelegateNode => AgentControlVerbV1::ManagerDelegateNode,
            Self::RsiControlManagerEscalate => AgentControlVerbV1::ManagerEscalate,
            Self::RsiControlManagerListEscalations => AgentControlVerbV1::ManagerListEscalations,
            Self::RsiControlManagerResolveEscalation => {
                AgentControlVerbV1::ManagerResolveEscalation
            }
            Self::RsiControlEnqueueLandingSource => AgentControlVerbV1::EnqueueLandingSource,
            Self::RsiControlGetProviderStatus => AgentControlVerbV1::GetProviderStatus,
            Self::RsiControlSubmitJob => AgentControlVerbV1::SubmitJob,
            Self::RsiControlGetJob => AgentControlVerbV1::GetJob,
            Self::RsiControlListJobs => AgentControlVerbV1::ListJobs,
            Self::RsiControlCancelJob => AgentControlVerbV1::CancelJob,
            Self::RsiControlSendSatelliteMessage => AgentControlVerbV1::SendSatelliteMessage,
            Self::RsiControlGetDaemonInfo => AgentControlVerbV1::GetDaemonInfo,
            Self::RsiControlRequestDeploy => AgentControlVerbV1::RequestDeploy,
        }
    }
}

/// One immutable entry in the closed v1 catalog.
#[derive(Debug, Clone, Copy)]
pub struct AgentControlDescriptorV1 {
    pub verb: AgentControlVerbV1,
    pub method: &'static str,
    pub description: &'static str,
    parameters_json: &'static str,
    pub native_tool: Option<NativeAgentControlToolV1>,
}

impl AgentControlDescriptorV1 {
    /// Checked-in JSON Schema text used directly by native Harness tools and
    /// as the raw `parameters` object in deterministic CLI discovery output.
    #[must_use]
    pub const fn parameters_json(self) -> &'static str {
        self.parameters_json
    }

    /// Parsed schema value used by CodexAppServer dynamic-tool registration.
    #[must_use]
    pub fn parameters(self) -> Value {
        serde_json::from_str(self.parameters_json)
            .expect("checked-in agent-control parameter schema must be valid JSON")
    }

    /// Deterministic v1 discovery envelope.
    #[must_use]
    pub fn envelope_json(self) -> String {
        // Method names are fixed ASCII catalog constants. Embedding the
        // checked-in schema text avoids map-iteration or feature-dependent
        // key ordering while retaining a valid JSON document.
        format!(
            "{{\"schema_version\":{AGENT_CONTROL_SCHEMA_VERSION_V1},\"method\":\"{}\",\"parameters\":{}}}",
            self.method, self.parameters_json
        )
    }
}

impl AgentControlVerbV1 {
    /// #1235 (S1.a): the project-bound PM verbs that take an optional
    /// `project_id` target. Omitted means the caller's own project.
    #[must_use]
    pub const fn is_project_bound(self) -> bool {
        matches!(
            self,
            Self::CreateIssue
                | Self::ListIssues
                | Self::GetIssue
                | Self::UpdateIssue
                | Self::UpdateIssueStatus
                | Self::ArchiveIssue
                | Self::RestoreIssue
                | Self::ListIssueEvents
                | Self::ManagerLaunchIssueWorker
                | Self::ManagerPrepareControl
                | Self::ManagerCommitPreparedControl
                | Self::ManagerControl
                | Self::ManagerGetAction
                | Self::ManagerProgress
                | Self::ManagerInspect
                | Self::ManagerUpdate
                // #2311: a manager lists its project's workers; a portfolio seat names the project.
                | Self::WorkerRoster
                // S1.b: topology, deploy and landing.
                | Self::TopologyUpsert
                | Self::TopologyList
                | Self::TopologyGet
                | Self::TopologyExecute
                | Self::TopologyGetExecution
                | Self::TopologyInterrupt
                | Self::PushSatelliteManagerPolicy
                | Self::ReseatSatelliteManager
                | Self::RequestDeploy
                | Self::EnqueueLandingSource
                | Self::ListRollingQueue
                | Self::SubmitJob
        )
    }

    /// Exact, case-sensitive lookup in the closed Agent catalog.
    #[must_use]
    pub fn from_method_name(method: &str) -> Option<Self> {
        agent_control_catalog_v1()
            .iter()
            .find(|descriptor| descriptor.method == method)
            .map(|descriptor| descriptor.verb)
    }

    #[must_use]
    pub fn descriptor(self) -> &'static AgentControlDescriptorV1 {
        agent_control_catalog_v1()
            .iter()
            .find(|descriptor| descriptor.verb == self)
            .expect("every AgentControlVerbV1 variant must have one descriptor")
    }

    /// Validate the transport-independent portion of an advertised request.
    ///
    /// This deliberately stops before authorization and mutable-state checks:
    /// the daemon remains the authority boundary for those predicates.
    pub fn validate_params(self, value: &Value) -> Result<(), AgentControlParamErrorV1> {
        self.validate_params_inner(value)
            .map_err(|error| self.schema_error(value).unwrap_or(error))
    }

    /// Explain an advertised schema violation without changing DTO or authority policy.
    pub fn schema_error(self, value: &Value) -> Option<AgentControlParamErrorV1> {
        diagnostics::validate(value, &self.descriptor().parameters(), "params").err()
    }

    /// #2313: rewrite each registered deprecated parameter name to its
    /// canonical name so the schema (which requires the canonical name) judges
    /// the request the DTO will decode. A request naming both is left as sent;
    /// the DTO refuses it as a duplicate field.
    fn with_deprecated_aliases_canonicalized(self, value: &Value) -> Value {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            for (verb, alias, canonical) in DEPRECATED_PARAMETER_ALIASES {
                if *verb == self && !object.contains_key(*canonical) {
                    if let Some(moved) = object.remove(*alias) {
                        object.insert((*canonical).to_string(), moved);
                    }
                }
            }
        }
        value
    }

    fn validate_params_inner(self, value: &Value) -> Result<(), AgentControlParamErrorV1> {
        // #2312: a read verb's id may be a prefix; the daemon resolves it.
        let normalized = crate::agent_id_ref::with_prefixes_as_full_ids(self, value);
        let normalized = self.with_deprecated_aliases_canonicalized(&normalized);
        let value = &normalized;
        validate_object(value, self)?;
        macro_rules! decode {
            ($type:ty, $validate:expr) => {{
                let request: $type = serde_json::from_value(value.clone())
                    .map_err(|_| AgentControlParamErrorV1::params())?;
                $validate(&request).map_err(|_| AgentControlParamErrorV1::params())
            }};
        }
        match self {
            Self::GetAuthorityCatalog => decode!(
                crate::agent_authority_catalog::AgentGetAuthorityCatalogRequestV1,
                |r: &crate::agent_authority_catalog::AgentGetAuthorityCatalogRequestV1| r
                    .validate()
            ),
            Self::SpawnChild => decode!(
                crate::agent_coordination::AgentSpawnChildRequestV1,
                |r: &crate::agent_coordination::AgentSpawnChildRequestV1| r.validate()
            ),
            Self::ReserveSuccessor => decode!(
                crate::agent_coordination::AgentReserveSuccessorRequestV1,
                |r: &crate::agent_coordination::AgentReserveSuccessorRequestV1| r.validate()
            ),
            Self::GetProgress => decode!(
                crate::agent_coordination::AgentGetProgressParamsV1,
                |r: &crate::agent_coordination::AgentGetProgressParamsV1| {
                    if r.session_ids.len() > crate::agent_coordination::AGENT_PROGRESS_MAX_COHORT
                        || r.session_ids.iter().any(Uuid::is_nil)
                    {
                        Err("agent_progress_invalid_request")
                    } else {
                        Ok(())
                    }
                }
            ),
            Self::SendMessage => decode!(
                crate::agent_coordination::AgentSendMessageRequestV1,
                |r: &crate::agent_coordination::AgentSendMessageRequestV1| r.validate()
            ),
            Self::GetStatus | Self::Halt => Ok(()),
            Self::ContinueChild => decode!(
                crate::agent_coordination::AgentContinueChildRequestV1,
                |r: &crate::agent_coordination::AgentContinueChildRequestV1| r.validate()
            ),
            Self::ArchiveChild => decode!(
                crate::agent_coordination::AgentArchiveChildRequestV1,
                |r: &crate::agent_coordination::AgentArchiveChildRequestV1| r.validate()
            ),
            Self::ScheduleWake => validate_schedule_wake(value),
            Self::CancelWake => validate_cancel_wake(value),
            Self::ListWakes => validate_list_wakes(value),
            Self::CreateIssue => validate_create_issue(value),
            Self::ListIssues => decode!(
                crate::rpc::AgentListIssuesRequestV1,
                |r: &crate::rpc::AgentListIssuesRequestV1| r.validated_limit().map(drop)
            ),
            Self::GetIssue => decode!(
                crate::rpc::AgentGetIssueRequestV1,
                |r: &crate::rpc::AgentGetIssueRequestV1| r.validate()
            ),
            Self::UpdateIssue => decode!(
                crate::rpc::AgentUpdateIssueRequestV1,
                |r: &crate::rpc::AgentUpdateIssueRequestV1| r.validate()
            ),
            Self::UpdateIssueStatus => decode!(
                crate::rpc::AgentUpdateIssueStatusRequestV1,
                |r: &crate::rpc::AgentUpdateIssueStatusRequestV1| r.validate()
            ),
            Self::ArchiveIssue => decode!(
                crate::rpc::AgentArchiveIssueRequestV1,
                |r: &crate::rpc::AgentArchiveIssueRequestV1| r.validate()
            ),
            Self::RestoreIssue => decode!(
                crate::rpc::AgentRestoreIssueRequestV1,
                |r: &crate::rpc::AgentRestoreIssueRequestV1| r.validate()
            ),
            Self::ListIssueEvents => decode!(
                crate::types::IssueEventPageRequestV1,
                |r: &crate::types::IssueEventPageRequestV1| r.validated_limit().map(drop)
            ),
            Self::ManagerProgress => decode!(
                crate::harness_manager::AgentManagerProgressRequestV1,
                |r: &crate::harness_manager::AgentManagerProgressRequestV1| r.validate()
            ),
            Self::ManagerInbox => decode!(
                crate::harness_manager::AgentManagerInboxRequestV1,
                |r: &crate::harness_manager::AgentManagerInboxRequestV1| r.validate()
            ),
            Self::ManagerSend => decode!(
                crate::harness_manager::AgentManagerSendRequestV1,
                |r: &crate::harness_manager::AgentManagerSendRequestV1| {
                    crate::harness_manager::validate_manager_message(
                        r.epic_id,
                        &r.message,
                        &r.idempotency_key,
                    )
                }
            ),
            Self::ManagerReply => decode!(
                crate::harness_manager::AgentManagerReplyRequestV1,
                |r: &crate::harness_manager::AgentManagerReplyRequestV1| {
                    crate::harness_manager::validate_manager_message(
                        r.request_id,
                        &r.message,
                        &r.idempotency_key,
                    )
                }
            ),
            Self::ManagerNotify => decode!(
                crate::harness_manager::AgentManagerNotifyRequestV1,
                |r: &crate::harness_manager::AgentManagerNotifyRequestV1| r.validate()
            ),
            Self::ManagerInspect => decode!(
                crate::harness_manager_v2::AgentManagerInspectRequestV2,
                |r: &crate::harness_manager_v2::AgentManagerInspectRequestV2| r.validate()
            ),
            Self::ManagerUpdate => decode!(
                crate::harness_manager_v2::AgentManagerUpdateRequestV2,
                |r: &crate::harness_manager_v2::AgentManagerUpdateRequestV2| r.validate()
            ),
            Self::SubmitReviewReceipt => decode!(
                crate::harness_manager_v2::AgentSubmitReviewReceiptRequestV1,
                |r: &crate::harness_manager_v2::AgentSubmitReviewReceiptRequestV1| r
                    .validate_wire()
            ),
            Self::ManagerControl => decode!(
                crate::harness_manager_v2::AgentManagerControlRequestV2,
                |r: &crate::harness_manager_v2::AgentManagerControlRequestV2| r.validate()
            ),
            Self::ManagerPrepareControl => decode!(
                crate::harness_manager_v2::AgentManagerPrepareControlRequestV2,
                |r: &crate::harness_manager_v2::AgentManagerPrepareControlRequestV2| r.validate()
            ),
            Self::ManagerCommitPreparedControl => decode!(
                crate::harness_manager_v2::AgentManagerCommitPreparedControlRequestV2,
                |r: &crate::harness_manager_v2::AgentManagerCommitPreparedControlRequestV2| r
                    .validate()
            ),
            Self::ManagerGetAction => decode!(
                crate::harness_manager_v2::AgentManagerGetActionRequestV2,
                |r: &crate::harness_manager_v2::AgentManagerGetActionRequestV2| r.validate()
            ),
            Self::ManagerLaunchIssueWorker => {
                use crate::manager_issue_worker::{
                    AgentManagerLaunchIssueWorkerRequestV1,
                    MANAGER_ISSUE_WORKER_REVIEW_CONTINUATION_CONFLICT,
                };
                let request: AgentManagerLaunchIssueWorkerRequestV1 =
                    serde_json::from_value(value.clone())
                        .map_err(|_| AgentControlParamErrorV1::params())?;
                request.validate().map_err(|reason| {
                    let mut error = AgentControlParamErrorV1::params();
                    if reason == MANAGER_ISSUE_WORKER_REVIEW_CONTINUATION_CONFLICT {
                        error.path = Some("params.continue_from".into());
                        error.constraint = Some(reason.into());
                    }
                    error
                })
            }
            Self::ManagerWorkView => decode!(
                crate::harness_manager::AgentManagerWorkViewRequestV1,
                |r: &crate::harness_manager::AgentManagerWorkViewRequestV1| r.validate()
            ),
            Self::ManagerDelegateNode => decode!(
                crate::manager_nodes::DelegateManagerNodeRequestV1,
                |r: &crate::manager_nodes::DelegateManagerNodeRequestV1| r.validate()
            ),
            Self::ManagerEscalate => decode!(
                crate::harness_manager::AgentManagerEscalateInputV1,
                |r: &crate::harness_manager::AgentManagerEscalateInputV1| r.validate()
            ),
            Self::ManagerListEscalations => decode!(
                crate::harness_manager::AgentManagerListEscalationsRequestV1,
                |_r: &crate::harness_manager::AgentManagerListEscalationsRequestV1| Ok::<
                    (),
                    &'static str,
                >(
                    ()
                )
            ),
            Self::ManagerResolveEscalation => decode!(
                crate::harness_manager::AgentManagerResolveEscalationRequestV1,
                |r: &crate::harness_manager::AgentManagerResolveEscalationRequestV1| r.validate()
            ),
            Self::TopologyUpsert => decode!(
                crate::topology_agent::AgentTopologyUpsertRequestV1,
                |r: &crate::topology_agent::AgentTopologyUpsertRequestV1| r.validate()
            ),
            Self::TopologyList => decode!(
                crate::topology_agent::AgentTopologyListRequestV1,
                |r: &crate::topology_agent::AgentTopologyListRequestV1| r
                    .validated_limit()
                    .map(drop)
            ),
            Self::TopologyGet => decode!(
                crate::topology_agent::AgentTopologyGetRequestV1,
                |r: &crate::topology_agent::AgentTopologyGetRequestV1| r.validate()
            ),
            Self::TopologyExecute => decode!(
                crate::topology_agent::AgentTopologyExecuteRequestV1,
                |r: &crate::topology_agent::AgentTopologyExecuteRequestV1| r.validate()
            ),
            Self::TopologyGetExecution => decode!(
                crate::topology_agent::AgentTopologyGetExecutionRequestV1,
                |r: &crate::topology_agent::AgentTopologyGetExecutionRequestV1| r
                    .validated_limit()
                    .map(drop)
            ),
            Self::TopologyInterrupt => decode!(
                crate::topology_agent::AgentTopologyInterruptRequestV1,
                |r: &crate::topology_agent::AgentTopologyInterruptRequestV1| r.validate()
            ),
            Self::TopologyResolveAttempt => decode!(
                crate::rpc::ResolveTopologyAttemptParams,
                crate::topology_agent::validate_resolve_attempt
            ),
            Self::EnqueueLandingSource => decode!(
                crate::rolling_queue::AgentEnqueueLandingSourceRequestV1,
                |r: &crate::rolling_queue::AgentEnqueueLandingSourceRequestV1| r.validate()
            ),
            Self::ListRollingQueue => decode!(
                crate::rolling_queue::AgentListRollingQueueRequestV1,
                |r: &crate::rolling_queue::AgentListRollingQueueRequestV1| r.validate()
            ),
            Self::CancelRollingQueueEntry => decode!(
                crate::rolling_queue::AgentCancelRollingQueueEntryRequestV1,
                |r: &crate::rolling_queue::AgentCancelRollingQueueEntryRequestV1| r.validate()
            ),
            Self::CheckLandingCandidate => decode!(
                crate::agent_landing_candidate::AgentCheckLandingCandidateRequestV1,
                |r: &crate::agent_landing_candidate::AgentCheckLandingCandidateRequestV1| r
                    .validate()
            ),
            Self::ReadSessionEvents => decode!(
                crate::agent_session_events::AgentReadSessionEventsRequestV1,
                |r: &crate::agent_session_events::AgentReadSessionEventsRequestV1| r.validate()
            ),
            Self::GetProviderStatus => decode!(
                crate::agent_provider_status::AgentGetProviderStatusRequestV1,
                |r: &crate::agent_provider_status::AgentGetProviderStatusRequestV1| r.validate()
            ),
            Self::SubmitJob => {
                let request: crate::agent_jobs::AgentSubmitJobRequestV1 =
                    serde_json::from_value(value.clone())
                        .map_err(|_| AgentControlParamErrorV1::params())?;
                request.typed_params().map(drop).map_err(|code| {
                    // Name the reason (an unallowed build feature, an oversize
                    // filter) instead of an opaque `invalid_input`.
                    let mut error = AgentControlParamErrorV1::params();
                    let detail = request.refusal_detail(code);
                    if detail != code {
                        error.constraint = Some(detail);
                    }
                    error
                })
            }
            Self::GetJob => decode!(
                crate::agent_jobs::AgentGetJobRequestV1,
                |_: &crate::agent_jobs::AgentGetJobRequestV1| Ok::<(), ()>(())
            ),
            Self::ListJobs => decode!(
                crate::agent_jobs::AgentListJobsRequestV1,
                |_: &crate::agent_jobs::AgentListJobsRequestV1| Ok::<(), ()>(())
            ),
            Self::CancelJob => decode!(
                crate::agent_jobs::AgentCancelJobRequestV1,
                |_: &crate::agent_jobs::AgentCancelJobRequestV1| Ok::<(), ()>(())
            ),
            Self::SendSatelliteMessage => decode!(
                crate::satellite_dispatch::AgentSendSatelliteMessageRequestV1,
                |r: &crate::satellite_dispatch::AgentSendSatelliteMessageRequestV1| r.validate()
            ),
            Self::ReportToHub => decode!(
                crate::satellite_dispatch::AgentReportToHubRequestV1,
                |r: &crate::satellite_dispatch::AgentReportToHubRequestV1| r.validate()
            ),
            Self::GetDaemonInfo => decode!(
                crate::agent_daemon_info::AgentGetDaemonInfoRequestV1,
                |_: &crate::agent_daemon_info::AgentGetDaemonInfoRequestV1| Ok::<(), ()>(())
            ),
            Self::QueryFailureSignatures => decode!(
                crate::agent_failure_signatures::AgentQueryFailureSignaturesRequestV1,
                |r: &crate::agent_failure_signatures::AgentQueryFailureSignaturesRequestV1| r
                    .validate()
            ),
            Self::WorkerRoster => decode!(
                crate::agent_worker_roster::AgentWorkerRosterRequestV1,
                |r: &crate::agent_worker_roster::AgentWorkerRosterRequestV1| r.validate()
            ),
            Self::PushSatelliteManagerPolicy => decode!(
                crate::satellite_manager_policy::AgentPushSatelliteManagerPolicyRequestV1,
                |r: &crate::satellite_manager_policy::AgentPushSatelliteManagerPolicyRequestV1| r
                    .validate()
            ),
            Self::ReseatSatelliteManager => decode!(
                crate::manager_reseat::AgentReseatSatelliteManagerRequestV1,
                |r: &crate::manager_reseat::AgentReseatSatelliteManagerRequestV1| r.validate()
            ),
            Self::RequestDeploy => decode!(
                crate::agent_deploy::AgentRequestDeployRequestV1,
                |r: &crate::agent_deploy::AgentRequestDeployRequestV1| r.validate()
            ),
            Self::SwitchHostPreset => decode!(
                crate::host_tuning::SwitchHostPreset,
                |r: &crate::host_tuning::SwitchHostPreset| r.validate()
            ),
            Self::GlobalOverview => decode!(
                crate::global_manager::AgentGlobalOverviewRequestV1,
                |_: &crate::global_manager::AgentGlobalOverviewRequestV1| Ok::<(), ()>(())
            ),
            Self::GlobalSend => decode!(
                crate::global_manager::AgentGlobalSendRequestV1,
                |r: &crate::global_manager::AgentGlobalSendRequestV1| r.validate()
            ),
            Self::GlobalAppointManager => decode!(
                crate::global_manager::AgentGlobalAppointManagerRequestV1,
                |r: &crate::global_manager::AgentGlobalAppointManagerRequestV1| r.validate()
            ),
            Self::ReportToGlobal => decode!(
                crate::global_manager::AgentReportToGlobalRequestV1,
                |r: &crate::global_manager::AgentReportToGlobalRequestV1| r.validate()
            ),
            Self::ReportUp => decode!(
                crate::manager_tier_routing::AgentReportUpRequestV1,
                |r: &crate::manager_tier_routing::AgentReportUpRequestV1| r.validate()
            ),
            Self::SendDown => decode!(
                crate::manager_tier_routing::AgentSendDownRequestV1,
                |r: &crate::manager_tier_routing::AgentSendDownRequestV1| r.validate()
            ),
            Self::ManagerAppointChild => decode!(
                crate::portfolio_delegation::AgentManagerAppointChildRequestV1,
                |r: &crate::portfolio_delegation::AgentManagerAppointChildRequestV1| r.validate()
            ),
            Self::ManagerRevokeChild => decode!(
                crate::portfolio_delegation::AgentManagerRevokeChildRequestV1,
                |r: &crate::portfolio_delegation::AgentManagerRevokeChildRequestV1| r.validate()
            ),
            Self::ManagerOverview => decode!(
                crate::manager_node_workspace::AgentManagerOverviewRequestV1,
                |_: &crate::manager_node_workspace::AgentManagerOverviewRequestV1| Ok::<(), ()>(())
            ),
            Self::CreateProject => decode!(
                crate::agent_projects::AgentCreateProjectRequestV1,
                |r: &crate::agent_projects::AgentCreateProjectRequestV1| r.validate()
            ),
            Self::UpdateProject => decode!(
                crate::agent_projects::AgentUpdateProjectRequestV1,
                |r: &crate::agent_projects::AgentUpdateProjectRequestV1| r.validate()
            ),
            Self::ListSourceWorktreeCohorts => decode!(
                crate::cohort_settlement::ListSourceWorktreeCohortsParams,
                |_r: &crate::cohort_settlement::ListSourceWorktreeCohortsParams| Ok::<(), String>(
                    ()
                )
            ),
            Self::AuditSourceWorktreeCohort => decode!(
                crate::cohort_settlement::AgentAuditSourceWorktreeCohortParams,
                |r: &crate::cohort_settlement::AgentAuditSourceWorktreeCohortParams| r.validate()
            ),
            Self::ApplySourceWorktreeCohort => decode!(
                crate::cohort_settlement::ApplySourceWorktreeCohortParams,
                |r: &crate::cohort_settlement::ApplySourceWorktreeCohortParams| r.validate()
            ),
            Self::OperatorTodoUpsert => decode!(
                crate::operator_todos::AgentOperatorTodoUpsertRequestV1,
                |r: &crate::operator_todos::AgentOperatorTodoUpsertRequestV1| r.validate()
            ),
            Self::OperatorTodoClose => decode!(
                crate::operator_todos::AgentOperatorTodoCloseRequestV1,
                |r: &crate::operator_todos::AgentOperatorTodoCloseRequestV1| r.validate()
            ),
            Self::StandingDirective => decode!(
                crate::standing_directives::AgentStandingDirectiveRequestV1,
                |r: &crate::standing_directives::AgentStandingDirectiveRequestV1| r.validate()
            ),
        }
    }
}

/// Stable validation envelope with bounded scalar schema diagnostics.
/// Legacy class and field remain unchanged; containers are never echoed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentControlParamErrorV1 {
    pub class: &'static str,
    pub field: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub value_truncated: bool,
}

impl AgentControlParamErrorV1 {
    /// Add diagnostics while preserving every existing envelope key.
    pub fn enrich(&self, envelope: &mut Value) {
        if !envelope.is_object() {
            *envelope = if envelope.is_null() {
                serde_json::json!({})
            } else {
                serde_json::json!({"detail": envelope.clone()})
            };
        }
        let object = envelope.as_object_mut().expect("object envelope");
        object
            .entry("code")
            .or_insert_with(|| Value::String("invalid_input".into()));
        for (key, value) in serde_json::to_value(self)
            .expect("validation error serializes")
            .as_object()
            .expect("error object")
        {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    const fn params() -> Self {
        Self {
            class: "invalid_input",
            field: "params",
            path: None,
            constraint: None,
            value: None,
            value_truncated: false,
        }
    }
}

fn validate_object(
    value: &Value,
    verb: AgentControlVerbV1,
) -> Result<(), AgentControlParamErrorV1> {
    let object = value
        .as_object()
        .ok_or_else(AgentControlParamErrorV1::params)?;
    let schema = verb.descriptor().parameters();
    let properties = schema["properties"]
        .as_object()
        .ok_or_else(AgentControlParamErrorV1::params)?;
    if object.keys().any(|key| !properties.contains_key(key))
        || diagnostics::name_group_violation(object, &schema).is_some()
        || schema["required"].as_array().is_some_and(|required| {
            required
                .iter()
                .any(|key| key.as_str().is_none_or(|key| !object.contains_key(key)))
        })
    {
        return Err(AgentControlParamErrorV1::params());
    }
    Ok(())
}

fn validate_schedule_wake(value: &Value) -> Result<(), AgentControlParamErrorV1> {
    let request: AgentScheduleWakeParams =
        serde_json::from_value(value.clone()).map_err(|_| AgentControlParamErrorV1::params())?;
    if request.message.trim().is_empty()
        || request.message.len() > 262_144
        || request.message.contains('\0')
        || !matches!(
            request.mode.as_deref(),
            Some("fresh" | "resume" | "on_terminal" | "program_guard" | "when")
        )
        || request.in_seconds.is_some_and(|seconds| seconds < 1)
        || request.every_seconds.is_some_and(|seconds| seconds < 1)
    {
        return Err(AgentControlParamErrorV1::params());
    }
    Ok(())
}

fn validate_cancel_wake(value: &Value) -> Result<(), AgentControlParamErrorV1> {
    let request: AgentCancelWakeParams =
        serde_json::from_value(value.clone()).map_err(|_| AgentControlParamErrorV1::params())?;
    if request.selector_is_valid() {
        Ok(())
    } else {
        Err(AgentControlParamErrorV1::params())
    }
}

fn validate_list_wakes(value: &Value) -> Result<(), AgentControlParamErrorV1> {
    let request: AgentListWakesParams =
        serde_json::from_value(value.clone()).map_err(|_| AgentControlParamErrorV1::params())?;
    if request.limit_is_valid() {
        Ok(())
    } else {
        Err(AgentControlParamErrorV1::params())
    }
}

fn validate_create_issue(value: &Value) -> Result<(), AgentControlParamErrorV1> {
    let request: crate::rpc::AgentCreateIssueParams =
        serde_json::from_value(value.clone()).map_err(|_| AgentControlParamErrorV1::params())?;
    let key = request.idempotency_key.as_bytes();
    if request.title.trim().is_empty()
        || request.title.len() > 512
        || request.title.contains('\0')
        || request.body.len() > 65_536
        || request.body.contains('\0')
        || request
            .priority
            .is_some_and(|priority| !(1..=4).contains(&priority))
        || request.labels.len() > 64
        || request
            .labels
            .iter()
            .any(|label| label.len() > 128 || label.contains('\0'))
        || request
            .assignee
            .as_ref()
            .is_some_and(|assignee| assignee.len() > 256 || assignee.contains('\0'))
        || key.is_empty()
        || key.len() > 128
        || key.contains(&0)
    {
        return Err(AgentControlParamErrorV1::params());
    }
    Ok(())
}

/// Supported params for `AgentGetStatus`. Omitted `session_id` targets the
/// transport-bound caller. The daemon retains its historical ignored-unknown
/// behavior; the catalog documents the narrower supported input surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGetStatusParams {
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

/// Supported params for `AgentHalt`. Omitted `session_id` targets the
/// transport-bound caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHaltParams {
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

/// Supported params for the caller-bound `AgentScheduleWake` surface.
///
/// `wake_session_id` is deliberately absent: the daemon binds the wake target
/// to the authenticated caller. `mode` remains optional at deserialization so
/// the existing stable runtime validation error is preserved when omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScheduleWakeParams {
    pub message: String,
    #[serde(default)]
    pub in_seconds: Option<i64>,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub every_seconds: Option<i64>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub watch_session_id: Option<String>,
    /// `mode:"when"` only: the daemon-evaluated wait predicate (#1006).
    #[serde(default)]
    pub when: Option<crate::wake_predicate::WakePredicate>,
    /// `mode:"when"` only: fire with `timed_out: true` after this many seconds
    /// if the predicate is still pending.
    #[serde(default)]
    pub timeout_seconds: Option<i64>,
}

/// Supported params for the caller-bound `AgentCancelWake` surface.
///
/// Exactly one selector is required. Ownership is the authenticated caller's
/// session only; no field can name another session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCancelWakeParams {
    #[serde(default)]
    pub job_id: Option<Uuid>,
    #[serde(default)]
    pub name: Option<String>,
}

impl AgentCancelWakeParams {
    /// Exactly one of `job_id` or a nonblank bounded `name`.
    #[must_use]
    pub fn selector_is_valid(&self) -> bool {
        match (&self.job_id, &self.name) {
            (Some(job_id), None) => !job_id.is_nil(),
            (None, Some(name)) => {
                !name.trim().is_empty() && name.len() <= 256 && !name.contains('\0')
            }
            _ => false,
        }
    }
}

/// Default and maximum page size of `AgentListWakes`.
pub const LIST_WAKES_DEFAULT_LIMIT: u32 = 64;
pub const LIST_WAKES_MAX_LIMIT: u32 = 256;

/// Supported params for the `AgentListWakes` surface. The owner is the
/// authenticated caller's session unless `session_id` names a live worker the
/// caller's `AgentWorkerRoster` scope covers (#2418).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListWakesParams {
    /// Also return disabled (cancelled, fired or suspended) jobs. Default false.
    #[serde(default)]
    pub include_disabled: bool,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Read another session's wakes (#2418). Omitted or the caller's own id
    /// reads the caller's wakes. Any other id must be a live worker the caller
    /// could see in `AgentWorkerRoster`; anything else is refused
    /// `list_wakes_target_not_authorized`. Read-only: no wake is created,
    /// cancelled or changed through it.
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

impl AgentListWakesParams {
    #[must_use]
    pub fn limit_is_valid(&self) -> bool {
        self.limit
            .is_none_or(|limit| (1..=LIST_WAKES_MAX_LIMIT).contains(&limit))
    }
}

const LIST_SOURCE_WORKTREE_COHORTS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {}
}"##;
const AUDIT_SOURCE_WORKTREE_COHORT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "repository_identity"
  ],
  "properties": {
    "after_custody_id": {
      "type": "string",
      "format": "uuid",
      "description": "Exclusive custody-key page hint from traversal.last_custody_id. Omit to restart. Apply must repeat the audited hint; never authorizes unexamined roots."
    },
    "fields": {
      "type": "string",
      "enum": ["summary", "full"],
      "description": "summary: counts by disposition, apply inputs and only eligible items; full: complete audit. Omitted: summary when nothing is applyable, full otherwise."
    },
    "repository_identity": {
      "type": "string",
      "minLength": 1,
      "maxLength": 4096
    }
  }
}"##;
const APPLY_SOURCE_WORKTREE_COHORT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "repository_identity",
    "plan_digest",
    "authorization",
    "idempotency_key"
  ],
  "properties": {
    "after_custody_id": {
      "type": "string",
      "format": "uuid",
      "description": "Exclusive custody-key page hint from traversal.last_custody_id. Omit to restart. Apply must repeat the audited hint; never authorizes unexamined roots."
    },
    "repository_identity": {
      "type": "string",
      "minLength": 1,
      "maxLength": 4096
    },
    "plan_digest": {
      "type": "string",
      "pattern": "^sha256:[0-9a-f]{64}$"
    },
    "authorization": {
      "type": "string",
      "description": "Exact authorization_phrase returned by the audit"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    }
  }
}"##;

const SPAWN_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "kind",
    "query",
    "idempotency_key"
  ],
  "properties": {
    "kind": {
      "type": "string",
      "enum": [
        "Story",
        "Task",
        "Bug",
        "Feature",
        "Refactor",
        "Research"
      ],
      "description": "Child session kind"
    },
    "provider": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "Claude",
        "Codex",
        "Pioneer",
        "OpenRouter",
        "Bedrock",
        "Local",
        "Antigravity",
        "CodexAppServer",
        "Harness",
        "Gemini",
        null
      ],
      "default": null,
      "description": "Optional child provider; omit to inherit the caller provider"
    },
    "model": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional model override for the child"
    },
    "effort": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional reasoning-effort hint"
    },
    "agent_role": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 64,
      "default": null,
      "description": "Optional normalized display role; caller and Epic authority remain transport-bound"
    },
    "query": {
      "type": "string",
      "minLength": 1,
      "maxLength": 262144,
      "description": "The child's initial prompt or task"
    },
    "topology_node": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional bound topology node id"
    },
    "iteration": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 0,
      "maximum": 4294967295,
      "default": null,
      "description": "Optional iteration override; omit to auto-increment"
    },
    "tags": {
      "type": [
        "array",
        "null"
      ],
      "maxItems": 64,
      "items": {
        "type": "string"
      },
      "default": null,
      "description": "Optional tag override set"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Required stable dedup key; exact retries return the same request and child IDs"
    }
  }
}"##;
const RESERVE_SUCCESSOR_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "kind",
    "query",
    "idempotency_key"
  ],
  "properties": {
    "kind": {
      "type": "string",
      "enum": [
        "Story",
        "Task",
        "Bug",
        "Feature",
        "Refactor",
        "Research"
      ],
      "description": "Successor session kind"
    },
    "model": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional model override for the successor"
    },
    "effort": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional reasoning-effort hint"
    },
    "query": {
      "type": "string",
      "minLength": 1,
      "maxLength": 262144,
      "description": "The successor's initial prompt or task"
    },
    "topology_node": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional bound topology node id"
    },
    "iteration": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 0,
      "maximum": 4294967295,
      "default": null,
      "description": "Optional iteration override; omit to auto-increment"
    },
    "tags": {
      "type": [
        "array",
        "null"
      ],
      "maxItems": 64,
      "items": {
        "type": "string"
      },
      "default": null,
      "description": "Optional tag override set"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Required stable dedup key; exact retries return the same reservation and successor IDs"
    }
  }
}"##;
const PROGRESS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "session_ids": {
      "type": "array",
      "maxItems": 256,
      "items": {
        "type": "string",
        "pattern": "^[0-9a-fA-F-]{8,36}$"
      },
      "default": [],
      "description": "Optional authorized child subdivision; omit for the full cohort. Each entry is a session UUID or a unique prefix of at least 8 hex characters inside your scope"
    }
  }
}"##;
const SEND_MESSAGE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "x-exactly-one-of": [
    [
      "session_id",
      "target_session_id"
    ]
  ],
  "required": [
    "message",
    "idempotency_key"
  ],
  "properties": {
    "session_id": {
      "type": "string",
      "format": "uuid",
      "description": "The child session to queue mail for"
    },
    "target_session_id": {
      "type": "string",
      "format": "uuid",
      "deprecated": true,
      "description": "Deprecated alias of session_id, accepted for one release (#2313); send session_id"
    },
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 16384,
      "description": "Message body queued for the target agent; acceptance does not prove delivery"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Required stable dedup key; exact retries return the original receipt and deadline"
    },
    "expires_at": {
      "type": [
        "string",
        "null"
      ],
      "format": "date-time",
      "default": null,
      "description": "Optional RFC3339 deadline; omit or send null for 30 minutes after first acceptance, or a bounded six hours when the recipient owns an armed result wake or open daemon job. An explicit deadline is preserved"
    }
  }
}"##;
const READ_SESSION_EVENTS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "session_id"
  ],
  "properties": {
    "session_id": {
      "type": "string",
      "pattern": "^[0-9a-fA-F-]{8,36}$",
      "description": "A session you may observe: your own child, a child of an Epic you lead, or a session in your manager scope. A UUID or a unique prefix of at least 8 hex characters inside that scope"
    },
    "after_sequence": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 0,
      "default": null,
      "description": "Forward page: events with a greater sequence, ascending. Omit for the tail (newest events, ascending)."
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 100,
      "default": 20
    },
    "event_types": {
      "type": [
        "array",
        "null"
      ],
      "maxItems": 8,
      "items": {
        "type": "string",
        "enum": [
          "Message",
          "ToolUse",
          "ToolResult",
          "System",
          "Thinking",
          "Compressed",
          "CompletionGate"
        ]
      },
      "default": null,
      "description": "Only these event types"
    },
    "max_bytes": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1024,
      "maximum": 262144,
      "default": 32768,
      "description": "Page byte budget; content, tool_input and metadata are also clipped per event"
    },
    "final_message_full": {
      "type": "boolean",
      "default": false,
      "description": "Return final_message.content up to 32768 characters instead of the default 8000-character cap"
    },
    "full_fields": {
      "type": "boolean",
      "default": false,
      "description": "Raise the per-event cap on content, tool_input and metadata from 2000 to 262144 characters so a long tool call (a Write over 2 KB) is exported whole; max_bytes still bounds the page and an event over the raised cap stays truncated:true"
    }
  }
}"##;
const RESEAT_SATELLITE_MANAGER_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "peer_id",
    "remote_session_id",
    "provider",
    "reason"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null
    },
    "peer_id": {
      "type": "string",
      "format": "uuid"
    },
    "remote_session_id": {
      "type": "string",
      "format": "uuid",
      "description": "Operator-declared remote manager scope address (the failed seat's session id, or a declared project id); the satellite resolves it to the live seat, which keeps resolving to the successor after the reseat"
    },
    "provider": {
      "type": "string",
      "enum": [
        "Claude",
        "Codex",
        "Pioneer",
        "OpenRouter",
        "Bedrock",
        "Local",
        "Antigravity",
        "CodexAppServer",
        "Harness"
      ],
      "description": "Provider of the successor seat"
    },
    "model": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Model of the successor; omitted means the project's default for that provider"
    },
    "effort": {
      "type": [
        "string",
        "null"
      ],
      "default": null
    },
    "reason": {
      "type": "string",
      "minLength": 1,
      "maxLength": 512,
      "description": "Why the seat is replaced; recorded on the satellite"
    },
    "provider_unavailable": {
      "type": "boolean",
      "default": false,
      "description": "Assert the seat's provider is unavailable; required to replace a seat that is Completed or Interrupted rather than Failed"
    }
  }
}"##;
const PUSH_SATELLITE_POLICY_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "peer_id",
    "remote_session_id",
    "expected_scope_version",
    "expected_policy_version",
    "idempotency_key",
    "policy"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null
    },
    "peer_id": {
      "type": "string",
      "format": "uuid"
    },
    "remote_session_id": {
      "type": "string",
      "format": "uuid",
      "description": "Operator-declared remote manager scope address: a session id, or a declared project id (the satellite resolves that project's current appointed manager, so it follows rotation and re-appointment)"
    },
    "expected_scope_version": {
      "type": "integer",
      "minimum": 1
    },
    "expected_policy_version": {
      "type": "integer",
      "minimum": 0
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    },
    "policy": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "allowed_launches",
        "max_created_sessions",
        "max_active_sessions",
        "provider_limits"
      ],
      "properties": {
        "allowed_launches": {
          "type": "array",
          "minItems": 1,
          "maxItems": 64,
          "items": {
            "type": "object",
            "additionalProperties": false,
            "required": [
              "provider",
              "model"
            ],
            "properties": {
              "provider": {
                "type": "string"
              },
              "model": {
                "type": "string",
                "minLength": 1
              },
              "effort": {
                "type": [
                  "string",
                  "null"
                ],
                "default": null
              }
            }
          }
        },
        "max_created_sessions": {
          "type": "integer",
          "minimum": 0,
          "maximum": 1024
        },
        "max_active_sessions": {
          "type": "integer",
          "minimum": 1,
          "maximum": 100
        },
        "provider_limits": {
          "type": "array",
          "items": {
            "type": "object",
            "additionalProperties": false,
            "required": [
              "provider",
              "max_active"
            ],
            "properties": {
              "provider": {
                "type": "string"
              },
              "max_active": {
                "type": "integer",
                "minimum": 1,
                "maximum": 64
              }
            }
          }
        }
      }
    }
  }
}"##;

const REQUEST_DEPLOY_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "sha",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "sha": {
      "type": "string",
      "minLength": 40,
      "maxLength": 40,
      "pattern": "^[0-9a-f]{40}$",
      "description": "Full lowercase commit SHA the binaries were built from"
    },
    "binaries_dir": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Absolute directory of freshly built binaries (rsid required); under the sandbox base, ~/.rsi/staging or ~/.cargo/shared-target"
    },
    "build": {
      "type": [
        "boolean",
        "null"
      ],
      "default": null,
      "description": "Reserved: build at sha through the job path; refused deploy_build_not_supported"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Required stable dedup key; an exact retry returns the same deploy"
    },
    "max_wait_secs": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 3600,
      "default": null,
      "description": "Bound on the wait for a quiet point; default 900"
    },
    "peer_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Deploy on this paired satellite over the hub link instead of on this daemon; binaries_dir is then a path on the satellite. Confirm with AgentGetDaemonInfo satellites"
    },
    "cancel": {
      "type": [
        "boolean",
        "null"
      ],
      "default": null,
      "description": "true cancels your own waiting deploy named by idempotency_key and sha (binaries_dir not needed): it settles failed with deploy_cancelled and its hold on new launches ends at once; deploy_cancel_too_late once the restart began or the deploy settled. With peer_id, forwards cancellation over the paired link and the satellite applies its own owner and idempotency checks; the hub stores no deploy state"
    },
    "interrupt_workers": {
      "type": [
        "boolean",
        "null"
      ],
      "default": null,
      "description": "Optional boolean: omitted defaults to true with peer_id, false for a local hub deploy. Explicit false waits for a lull. True stops waiting for mid-turn workers after deploy_drain_hold_secs; the restart interrupts and resumes them. Ordinary test/scoped_test jobs are never stopped or rerun: their units keep running across the restart and the new daemon re-adopts them (one wake each, when the job ends); landing, build and protected jobs still block. The receipt names expected_interrupted_workers, expected_surviving_jobs, interrupted_workers and surviving_jobs; the outcome names interrupted sessions and the jobs that kept running. Not with cancel. A peer without deploy_interrupt is refused satellite_deploy_interrupt_unsupported; explicit false remains compatible"
    }
  }
}"##;
const QUERY_FAILURE_SIGNATURES_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "test_id": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 512,
      "default": null,
      "description": "Exact libtest/nextest test id, e.g. session::launch::tests::x"
    },
    "digest": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^[0-9a-f]{64}$",
      "default": null,
      "description": "sha256 failure digest (rsi-known-failure block prints it); both fields must match when both are set"
    }
  }
}"##;
const WORKER_ROSTER_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "fields": {
      "type": ["string", "null"],
      "enum": ["summary", "full", null],
      "default": null,
      "description": "summary (default): live workers plus final_handoff/failed active within the last hour; older finished history is counted in hidden_finished. full or nonempty states restores matching history, including stale strands"
    },
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "The project whose workers to list: its appointed manager, or a portfolio seat whose grant covers it. Omit for your own project when you are its appointed manager; any other caller lists the workers it launched itself"
    },
    "states": {
      "type": "array",
      "items": {
        "type": "string",
        "enum": [
          "running",
          "parked_on_job",
          "parked_on_wake",
          "stranded",
          "final_handoff",
          "failed"
        ]
      },
      "maxItems": 6,
      "default": [],
      "description": "Only workers in these derived states, including all matching history; empty uses the summary view unless fields is full"
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 256,
      "default": null,
      "description": "Page size, default 64; workers come newest activity first"
    }
  }
}"##;
const MANAGER_OVERVIEW_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "fleet": {
      "type": "string",
      "enum": [
        "none",
        "summary",
        "full"
      ],
      "default": "summary",
      "description": "Fleet rollup detail: summary is one row per model (1h and 24h invocations, errors, cost) plus active and totals; full is every project, provider and model group over 5m/1h/24h"
    },
    "detail": {
      "type": "string",
      "enum": [
        "compact",
        "full"
      ],
      "default": "compact",
      "description": "compact (default) drops repeats: one seat digest per project child, standing directives as id/scope/title/updated_at, coordinator_budget once per distinct budget under coordinator_budgets, predecessors as a count plus the newest, one launches form (grant.allowed_launches); full is the whole workspace"
    },
    "directives": {
      "type": "boolean",
      "default": false,
      "description": "Return the full text of every standing directive (compact returns only id, scope, title, updated_at and text_bytes); implied by detail full"
    }
  }
}"##;
const CREATE_PROJECT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "name",
    "path"
  ],
  "properties": {
    "name": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Unique project name shown in the TUI tab bar"
    },
    "path": {
      "type": "string",
      "minLength": 1,
      "maxLength": 4096,
      "description": "Absolute directory of the repository or working tree; it must exist and sit inside a configured workspace root; with no workspace roots configured, a strict descendant of the daemon user's home directory (not the home directory itself, anything under ~/.rsi, or /)"
    },
    "description": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 2048,
      "default": null
    },
    "color": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^#[0-9a-fA-F]{6}$",
      "default": null,
      "description": "Tab color as #rrggbb"
    }
  }
}"##;
const UPDATE_PROJECT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "project_id"
  ],
  "properties": {
    "project_id": {
      "type": "string",
      "format": "uuid",
      "description": "A project in your coverage: your own project (project manager) or a project of your grant (portfolio seat)"
    },
    "name": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 128,
      "default": null
    },
    "path": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 4096,
      "default": null,
      "description": "New absolute directory; refused while the project has a live session"
    },
    "description": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 2048,
      "default": null
    },
    "color": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^#[0-9a-fA-F]{6}$",
      "default": null
    }
  }
}"##;
const OPERATOR_TODO_UPSERT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "todo_key",
    "title",
    "how",
    "idempotency_key"
  ],
  "properties": {
    "todo_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Stable slug for this item in your scope (for example codex-login); send it again to update the same open item"
    },
    "title": {
      "type": "string",
      "minLength": 1,
      "maxLength": 512,
      "description": "What only the operator can do, in one line"
    },
    "how": {
      "type": "string",
      "minLength": 1,
      "maxLength": 2048,
      "description": "The exact command or keys"
    },
    "link": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 512,
      "default": null,
      "description": "An Issue (issue:#N), a session (session:<uuid>) or a URL"
    },
    "priority": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 4,
      "default": null,
      "description": "1 is most urgent; 3 when omitted"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; a replay returns the same item"
    }
  }
}"##;
const STANDING_DIRECTIVE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "action"
  ],
  "properties": {
    "action": {
      "type": "string",
      "enum": [
        "set",
        "clear"
      ],
      "description": "set relays a directive to a scope below yours; clear ends one a manager relayed"
    },
    "scope": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "For set: the manager scope, project:<uuid>, area:<uuid> or portfolio:<uuid>, strictly below your own"
    },
    "text": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 2048,
      "default": null,
      "description": "For set: the operator's instruction, verbatim"
    },
    "provenance": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 512,
      "default": null,
      "description": "For set: where and when the operator said it (required for kind operator_relay); with kind manager an optional note on why"
    },
    "kind": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "operator_relay",
        "manager",
        null
      ],
      "default": null,
      "description": "For set: operator_relay (default) relays the operator's words; manager is your own standing ruling for the scope (#3736), shown to its seats as manager-authored, never as operator words"
    },
    "expires_in_secs": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 7776000,
      "default": null,
      "description": "For set: seconds until it expires; absent means until cleared"
    },
    "idempotency_key": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 128,
      "default": null,
      "description": "For set: replay key; a replay returns the same directive"
    },
    "directive_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "For clear: the directive to end"
    }
  }
}"##;
const OPERATOR_TODO_CLOSE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "status",
    "evidence",
    "idempotency_key"
  ],
  "properties": {
    "todo_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "The item to close; set exactly one of todo_id and todo_key"
    },
    "todo_key": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 128,
      "default": null,
      "description": "The key of an item in your own scope"
    },
    "status": {
      "type": "string",
      "enum": [
        "done",
        "dismissed"
      ]
    },
    "evidence": {
      "type": "string",
      "minLength": 1,
      "maxLength": 2048,
      "description": "What you observed (done) or why the item no longer applies (dismissed); recorded on the item"
    },
    "observed": {
      "type": "boolean",
      "default": false,
      "description": "Must be true for done: you observed that the thing was done. You never mark an item done on the operator's behalf"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; a replay returns the same closed item"
    }
  }
}"##;
const GLOBAL_OVERVIEW_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {}
}"##;
const MANAGER_LAUNCH_ISSUE_WORKER_BASE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "issue",
    "brief",
    "launch",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "issue": {
      "type": "integer",
      "minimum": 1,
      "description": "The Issue's display number in your project"
    },
    "brief": {
      "type": "string",
      "minLength": 1,
      "maxLength": 24576,
      "description": "The worker's task text; it reads its own Issue with AgentGetIssue, so do not paste the Issue. Launch selectors come from this admitted request.brief field, not the whole daemon-compiled visible query. References only in daemon-copied records grant no additional live Issue reads; explicitly select required same-project dependencies here (a valid #N) or in related."
    },
    "launch": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "provider",
        "model"
      ],
      "properties": {
        "provider": {
          "type": "string",
          "description": "Provider; must be in your policy's allowed_launches"
        },
        "model": {
          "type": "string",
          "minLength": 1,
          "maxLength": 256
        },
        "effort": {
          "type": [
            "string",
            "null"
          ],
          "maxLength": 32,
          "default": null
        }
      }
    },
    "parent_epic_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "The Epic to create the worker under; optional when your scope holds exactly one live Epic. With several, set it: a refusal manager_issue_worker_parent_required lists the candidate Epic ids"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; a replay returns the same worker"
    }
  }
}"##;
/// #1195: the base schema plus the shared `sandbox_source` property.
static MANAGER_LAUNCH_ISSUE_WORKER_SCHEMA: LazyLock<String> = LazyLock::new(|| {
    let mut schema: Value = serde_json::from_str(MANAGER_LAUNCH_ISSUE_WORKER_BASE_SCHEMA)
        .expect("launch issue worker schema is valid JSON");
    schema["properties"]["sandbox_source"] = serde_json::json!({
        "anyOf": [manager_v2::sandbox_source(), {"type": "null"}],
        "default": null
    });
    schema["properties"]["continue_from"] = serde_json::json!({
        "type": ["string", "null"],
        "format": "uuid",
        "default": null,
        "description": "#1254: the terminal worker of this Issue to continue after it passed the baton; the new worker branches from its committed HEAD, inherits its lineage and starts from its final message and the Issue's latest handoff. Exclusive with sandbox_source and review_of; omit continue_from to launch a fresh review with the latest implementer handoff."
    });
    schema["properties"]["review_of"] = serde_json::json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "default": null,
        "description": "#1590: display number of the implementer Issue this worker reviews. The daemon copies that Issue's text and its complete latest handoff (landing filters included) into the worker's brief. review_of alone grants no reverse live read of that Issue; explicit selection in the admitted request.brief field or related can independently grant it. References only in copied records grant no additional reads. Without sandbox_source, the latest handoff's declared commit pins the reviewer sandbox when available locally; otherwise it stays on origin/rolling and the brief disallows jobs for missing source tests. An explicit sandbox_source always wins. Do not paste the handoff yourself. Exclusive with continue_from; omit continue_from to launch a fresh review with the latest handoff. Explicit unknown sandbox_source commits are refused."
    });
    schema["properties"]["related"] = serde_json::json!({
        "type": "array",
        "items": {"type": "integer", "minimum": 1},
        "maxItems": 16,
        "uniqueItems": true,
        "default": [],
        "description": "#218: display numbers of same-project Issues whose COMPLETE text and latest handoff the daemon copies into the worker's brief as read-only context (a snapshot at launch; one too large for the brief is named, never cut, and the worker reads it with AgentGetIssue). The worker may also read each live. References only in these copied records grant no additional reads. Writes, status changes and handoffs stay on `issue`. Not the bound Issue; an unknown number is refused manager_issue_worker_related_unavailable. A `#N` or `#A-#B` in the admitted request.brief field (not the whole compiled query; a range wider than 32 selects only its endpoints), in the Issue's body or in its handoffs is also readable (read-only) without listing it here; more than 64 distinct selectors in the brief are refused manager_issue_worker_references_overflow. A plain relaunch of the same Issue (no continue_from) also carries over the related Issues of its earlier launches (#275; result carried_related, and skipped_related for one that no longer exists or is archived)."
    });
    schema["properties"]["qa_lane"] = serde_json::json!({
        "type": "boolean", "default": false,
        "description": "Explicit manager delegation of bounded QA shard jobs (5-90 minutes, two running per owner); default false, including continuation. Not inherited."
    });
    schema.to_string()
});
const GLOBAL_SEND_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "project_id",
    "message",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": "string",
      "format": "uuid",
      "description": "A project in your grant; the message goes to its current project manager"
    },
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 32768,
      "description": "The full request or note for that project manager, with acceptance criteria"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; replay only identical content under it"
    }
  }
}"##;
const GLOBAL_APPOINT_MANAGER_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "project_id",
    "launch",
    "query",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": "string",
      "format": "uuid",
      "description": "A project in your grant"
    },
    "launch": {
      "type": "object",
      "additionalProperties": false,
      "required": [
        "provider",
        "model"
      ],
      "properties": {
        "provider": {
          "type": "string",
          "enum": [
            "Claude",
            "Codex",
            "Pioneer",
            "OpenRouter",
            "Bedrock",
            "Local",
            "Antigravity",
            "CodexAppServer",
            "Harness",
            "Gemini"
          ],
          "description": "Provider; must be in your grant's allowed_launches"
        },
        "model": {
          "type": "string",
          "minLength": 1,
          "maxLength": 128,
          "description": "Model; must be in your grant's allowed_launches"
        },
        "effort": {
          "type": [
            "string",
            "null"
          ],
          "default": null,
          "description": "Effort; must match the allowlist entry when it names one"
        }
      },
      "description": "The new project manager's launch"
    },
    "query": {
      "type": "string",
      "minLength": 1,
      "maxLength": 65536,
      "description": "The new project manager's self-contained first prompt"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, e.g. gm-<project>-<purpose>-<n>; a replay returns the same session"
    },
    "sandbox": {
      "type": [
        "boolean",
        "null"
      ],
      "default": null,
      "description": "Launch in its own git worktree sandbox (default true)"
    }
  }
}"##;
/// #1239: `AgentManagerAppointChild`. The target is a covered project or a
/// child portfolio node; it has no parent or adopt field (only the operator
/// creates roots and re-parents).
static MANAGER_APPOINT_CHILD_SCHEMA: LazyLock<String> = LazyLock::new(|| {
    let global: Value = serde_json::from_str(GLOBAL_APPOINT_MANAGER_SCHEMA)
        .expect("the AgentGlobalAppointManager schema is valid JSON");
    let uuid = |description: &str| serde_json::json!({"type":["string","null"],"format":"uuid","default":null,"description":description});
    let policy = |description: &str| serde_json::json!({"type":["object","null"],"default":null,"description":description});
    let mut launch = global["properties"]["launch"].clone();
    launch["description"] =
        Value::from("The new seat's launch; must be in your grant's allowed_launches");
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["target", "launch", "query", "idempotency_key"],
        "properties": {
            "target": {
                "description": "Who the new session becomes: {kind:\"project\", project_id} appoints or replaces the project manager of a project in your coverage (saved with your child_policy); {kind:\"portfolio\", tier_label, project_ids, policy, ...} creates a child manager node over a strict subset of your coverage that narrows your grant in every dimension; {kind:\"portfolio\", node_id} replaces the seat of a child node you granted.",
                "oneOf": [
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "project_id"],
                        "properties": {
                            "kind": {"type": "string", "enum": ["project"]},
                            "project_id": {"type": "string", "format": "uuid", "description": "A project in your coverage"}
                        }
                    },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind"],
                        "properties": {
                            "kind": {"type": "string", "enum": ["portfolio"]},
                            "node_id": uuid("null creates a child node; a child node you granted replaces its seat; your own node id hands your own seat to the new session (then send no other target field but launch_project_id and expected_grant_version)"),
                            "tier_label": {"type": ["string", "null"], "minLength": 1, "maxLength": 32, "default": null, "description": "Display label of a new child (authority never reads it)"},
                            "project_ids": {"type": "array", "items": {"type": "string", "format": "uuid"}, "maxItems": 64, "default": [], "description": "A strict subset of your coverage, disjoint from your other children"},
                            "allowed_launches": {"type": "array", "items": launch.clone(), "maxItems": crate::global_manager::GLOBAL_MANAGER_MAX_LAUNCHES, "default": [], "description": "Launches the child may make (default: yours); a subset of yours"},
                            "policy": policy("The child's in-project policy (ManagerPolicyV2): capabilities within yours, every finite allowance strictly below yours"),
                            "child_policy": policy("The policy the child hands to the managers it appoints (within policy)"),
                            "max_direct_reports": {"type": ["integer", "null"], "minimum": 0, "maximum": 64, "default": null, "description": "At most yours (default: the smaller of 5 and yours)"},
                            "launch_project_id": uuid("The project the new seat runs in (default: the child's first project)"),
                            "expected_grant_version": {"type": ["integer", "null"], "minimum": 1, "default": null, "description": "Seat replacement only: the child's current grant version"}
                        }
                    }
                ]
            },
            "launch": launch,
            "query": {"type": "string", "minLength": 1, "maxLength": 65536, "description": "The new seat's self-contained first prompt"},
            "idempotency_key": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Replay key, scoped to your node; a replay returns the same session"},
            "sandbox": {"type": ["boolean", "null"], "default": null, "description": "Launch in its own git worktree sandbox (default true)"},
            "handoff": {"type": ["string", "null"], "minLength": 1, "maxLength": crate::portfolio_delegation::SEAT_HANDOFF_MAX_BYTES, "default": null, "description": "Seat replacement only: the repo path or commit of the handoff you wrote; recorded with the seat change"}
        }
    });
    schema.to_string()
});
const MANAGER_REVOKE_CHILD_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "node_id",
    "expected_grant_version",
    "idempotency_key"
  ],
  "properties": {
    "node_id": {
      "type": "string",
      "format": "uuid",
      "description": "A child node your node granted"
    },
    "expected_grant_version": {
      "type": "integer",
      "minimum": 1,
      "description": "The child's current grant version (CAS)"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; a replay against the revoked child returns it"
    }
  }
}"##;
const REPORT_TO_GLOBAL_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "message",
    "idempotency_key"
  ],
  "properties": {
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 32768,
      "description": "Your report to the global manager: landed work, a gate that blocks you, or your handoff path"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; replay only identical content under it"
    }
  }
}"##;
const REPORT_UP_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "message",
    "idempotency_key"
  ],
  "properties": {
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 32768,
      "description": "Your report to the manager one level above you: landed work, a gate that blocks you, or your handoff path"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; replay only identical content under it"
    }
  }
}"##;
const SEND_DOWN_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "target",
    "message",
    "idempotency_key"
  ],
  "properties": {
    "target": {
      "description": "The descendant manager node to message (a node below yours, inside your coverage)",
      "oneOf": [
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "kind",
            "node_id"
          ],
          "properties": {
            "kind": {
              "const": "portfolio"
            },
            "node_id": {
              "type": "string",
              "format": "uuid"
            }
          }
        },
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "kind",
            "project_id"
          ],
          "properties": {
            "kind": {
              "const": "project"
            },
            "project_id": {
              "type": "string",
              "format": "uuid"
            }
          }
        },
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "kind",
            "node_id"
          ],
          "properties": {
            "kind": {
              "const": "area"
            },
            "node_id": {
              "type": "string",
              "format": "uuid"
            }
          }
        }
      ]
    },
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 32768,
      "description": "The full request or note for that manager, with acceptance criteria"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key; replay only identical content under it"
    }
  }
}"##;
const GET_DAEMON_INFO_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {}
}"##;
const GET_PROVIDER_STATUS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "provider": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "claude",
        "codex",
        "pioneer",
        "openrouter",
        "bedrock",
        "local",
        "antigravity",
        "codex_app_server",
        "harness",
        "anthropic",
        "openai",
        null
      ],
      "default": null,
      "description": "Provider to report; omit for every provider"
    },
    "models": {
      "type": "array",
      "maxItems": 32,
      "items": {
        "type": "string",
        "minLength": 1,
        "maxLength": 128
      },
      "default": [],
      "description": "OpenRouter model ids to price (needs provider openrouter): the openrouter entry then carries model_prices (per-million-token prompt, completion and cache-read prices with fetched_at, from the daemon's own model discovery) and model_prices_unknown"
    },
    "run_budget_usd": {
      "type": [
        "number",
        "null"
      ],
      "exclusiveMinimum": 0,
      "default": null,
      "description": "A benchmark run's USD budget (needs provider openrouter): the openrouter entry then carries run_budget {admitted, refusal_code, status{run_budget_usd, spent_usd, remaining_usd, exhausted}}. Not admitted when the run spent its budget (stop launching tasks), what is left is larger than the cap's remainder, or the cap is reached or cannot be judged"
    },
    "run_sessions": {
      "type": "array",
      "maxItems": 256,
      "items": {
        "type": "string",
        "format": "uuid"
      },
      "default": [],
      "description": "The sessions the run launched so far (with run_budget_usd); their invocation spend is the run's spend. Empty for a run about to start"
    }
  }
}"##;
const STATUS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "session_id": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^[0-9a-fA-F-]{8,36}$",
      "default": null,
      "description": "Target session UUID, or a unique prefix of at least 8 hex characters inside your scope; omit to target this session"
    }
  }
}"##;
const TARGET_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Target session UUID; omit to target this session"
    }
  }
}"##;
const WAKE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "message",
    "mode"
  ],
  "properties": {
    "message": {
      "type": "string",
      "description": "Prompt to run when the wake fires"
    },
    "in_seconds": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Fire this many seconds from now; mutually exclusive with at"
    },
    "at": {
      "type": [
        "string",
        "null"
      ],
      "format": "date-time",
      "default": null,
      "description": "RFC3339 absolute fire time; mutually exclusive with in_seconds"
    },
    "name": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional human-readable job name"
    },
    "every_seconds": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Optional recurring interval in seconds"
    },
    "mode": {
      "type": "string",
      "enum": [
        "fresh",
        "resume",
        "on_terminal",
        "program_guard",
        "when"
      ],
      "description": "Required: fresh is a consumed, best-effort root launch after this session is terminal and transfers no hierarchy or lead authority; use AgentReserveSuccessor for master turnover. resume re-invokes this session with context; on_terminal arms a terminal watch; program_guard registers daemon-authoritative program identity; when arms ONE resume wake that the daemon fires when the `when` predicate is true (no model turn is spent polling) and carries each job's id, name, state, exit code and refusal"
    },
    "watch_session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Watched subject UUID; requires mode on_terminal and cannot steer the caller-bound wake target"
    },
    "when": {
      "type": [
        "object",
        "null"
      ],
      "default": null,
      "additionalProperties": false,
      "description": "Required with mode when (and only then): exactly one predicate the daemon evaluates. jobs_terminal fires once every listed AgentSubmitJob job you own or may watch through proven succession is terminal (submit them with wake none for one wake per batch); sha_on_rolling fires once that commit is an ancestor of origin/rolling in your repository (polled); shas_on_rolling accepts 1-32 distinct commits and fires once all are ancestors, reporting each SHA as on_rolling, pending or unknown (also on timeout or manual trigger); with fire \"each\" beside it the one wake instead resumes you once per landing (each firing lists landed_since_last_firing and marks those SHAs new=true) and retires after the last listed commit lands or the timeout passes, so watch every queued source with one wake, not one wake per SHA. A session holds at most 16 enabled when wakes (topology_execution_settled wakes are counted apart, up to 128); one more is refused wake_when_cap_reached. issue_closed fires once that Issue is Closed (the manager closes an Issue when its work is on rolling), so you can wait on a dependency without its SHA; a Cancelled or missing Issue wakes you as unsatisfiable. todo_closed fires once that operator to-do item (in your own manager scope) is done or dismissed and carries its status and evidence, so you wait on the operator without polling; an unknown or out-of-scope item is refused at scheduling time. topology_execution_settled fires once when that topology execution (one you requested, or one under an Epic you manage or lead) is succeeded, failed, cancelled or blocked, and carries its status, error, blocked-reason summary (kind, node_id, attempt_id) and finish time; an owner keeps exactly one enabled settle wake per execution: arming another for the same execution, under any name, replaces the earlier one (reported in replaced_job_ids); a blocked execution that is later resolved needs a new wake, and an unknown or out-of-scope id is refused at scheduling time. settlement_run_settled fires once when that source-worktree settlement run (the run_id AgentApplySourceWorktreeCohort returned while the run was still applying) is finished, every root settled, refused, recovery_required or unattempted, and carries the run state, per-phase counts and terminal error; an unknown run or one outside your cohort authority is refused at scheduling time. health_anomaly (manager seats only; a project manager watches its project, a portfolio seat every project) is the replacement for an hourly check-in turn: the daemon evaluates the PM slots, context fill against the rotation threshold, blocked or overrun topology work, hub disk and CPU temperature, satellite disk, provider faults and an untracked red rolling QA every minute with no model call and resumes you once with a condition=<kind> subject=<id> ignore_key=<kind>:<subject> detail=\"...\" line per anomaly, so a quiet daemon costs you no turn; re-arm after handling, and list a persisting condition's printed ignore_key value, verbatim, in ignore. in_seconds, at and every_seconds are not accepted with mode when",
      "properties": {
        "jobs_terminal": {
          "type": "array",
          "items": {
            "type": "string",
            "format": "uuid"
          },
          "minItems": 1,
          "maxItems": 32,
          "description": "Job ids you own or may watch through manager succession or a published Issue-worker continuation; an unknown or unrelated id is refused at scheduling time"
        },
        "sha_on_rolling": {
          "type": "string",
          "pattern": "^[0-9a-f]{40}$",
          "description": "Full lowercase 40-hex commit id"
        },
        "shas_on_rolling": {
          "type": "array",
          "items": {
            "type": "string",
            "pattern": "^[0-9a-f]{40}$"
          },
          "minItems": 1,
          "maxItems": 32,
          "uniqueItems": true,
          "description": "Full lowercase 40-hex commit ids; fires once all are ancestors of origin/rolling and reports each SHA's state"
        },
        "fire": {
          "type": "string",
          "enum": [
            "all",
            "each"
          ],
          "description": "shas_on_rolling only (wake_when_predicate_invalid otherwise): all (the default) fires once when every listed commit is on rolling; each fires once per landing, naming the commits newly on rolling, and stays armed until all have landed or the timeout passes"
        },
        "issue_closed": {
          "type": "integer",
          "minimum": 1,
          "description": "Display number (the N in #N) of an Issue in your project"
        },
        "todo_closed": {
          "type": "string",
          "format": "uuid",
          "description": "Id of an operator to-do item in your own manager scope; an unknown or out-of-scope id is refused at scheduling time"
        },
        "topology_execution_settled": {
          "type": "string",
          "format": "uuid",
          "description": "Id of a topology execution you requested or can read through its Epic; an unknown or out-of-scope id is refused at scheduling time"
        },
        "settlement_run_settled": {
          "type": "string",
          "format": "uuid",
          "description": "run_id of a source-worktree settlement run (from AgentApplySourceWorktreeCohort) you hold cohort authority over; an unknown or out-of-authority id is refused at scheduling time"
        },
        "health_anomaly": {
          "type": "object",
          "additionalProperties": false,
          "description": "Manager seats only (wake_when_health_anomaly_manager_only otherwise). {} watches everything your seat covers; the daemon sets the scope.",
          "properties": {
            "ignore": {
              "type": "array",
              "items": {
                "type": "string"
              },
              "maxItems": 32,
              "description": "keys (the ignore_key= value, verbatim, as the previous wake printed it) you accept and do not want to be woken for again"
            }
          }
        }
      }
    },
    "timeout_seconds": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 604800,
      "default": null,
      "description": "mode when only: fire with timed_out true after this many seconds if the predicate is still pending, so you are never stranded"
    }
  }
}"##;
const CANCEL_WAKE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "job_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Job id returned by AgentScheduleWake; must be one of your own jobs. Provide exactly one of job_id or name"
    },
    "name": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 256,
      "default": null,
      "description": "Name of your own scheduled job(s) to disable. Provide exactly one of job_id or name"
    }
  }
}"##;
const LIST_WAKES_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "include_disabled": {
      "type": "boolean",
      "default": false,
      "description": "Also return disabled (cancelled, fired or suspended) jobs"
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 256,
      "default": 64,
      "description": "Maximum jobs returned, soonest fire first"
    },
    "session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Read this live worker's wakes instead of your own. It must be a worker AgentWorkerRoster lists for your current scope (your own launches and the node sessions of topology executions you requested, or your project as its appointed manager, including its topology executions' node sessions, or a project inside a portfolio seat's grant); any other session is refused list_wakes_target_not_authorized. Read-only."
    }
  }
}"##;
const CREATE_ISSUE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "title",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "title": {
      "type": "string",
      "minLength": 1,
      "maxLength": 512
    },
    "body": {
      "type": "string",
      "maxLength": 65536,
      "default": ""
    },
    "priority": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 4,
      "default": null
    },
    "labels": {
      "type": "array",
      "maxItems": 64,
      "items": {
        "type": "string",
        "maxLength": 128
      },
      "default": []
    },
    "assignee": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 256,
      "default": null
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    },
    "harness": {
      "type": "boolean",
      "default": false,
      "description": "File in the RSI harness project instead of your own: use for a defect in RSI itself found while working in another project. Cannot combine with project_id"
    },
    "source_issue": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Only with harness:true: display number of the Issue in your own project this harness filing mirrors. Refused without harness. To link an Issue in your own project, omit this and cite it as #N in the body"
    }
  }
}"##;
const LIST_ISSUES_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "labels": {
      "type": "array",
      "maxItems": 16,
      "items": {
        "type": "string",
        "minLength": 1,
        "maxLength": 64
      },
      "default": [],
      "description": "Require all supplied labels (exact, case-sensitive)"
    },
    "status": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "Open",
        "InProgress",
        "Closed",
        "Cancelled",
        null
      ],
      "default": null
    },
    "archive": {
      "type": "string",
      "enum": [
        "Active",
        "Archived",
        "All"
      ],
      "default": "Active"
    },
    "cursor": {
      "type": [
        "object",
        "null"
      ],
      "additionalProperties": false,
      "required": [
        "display_number",
        "issue_id"
      ],
      "properties": {
        "display_number": {
          "type": "integer",
          "minimum": 1
        },
        "issue_id": {
          "type": "string",
          "format": "uuid"
        }
      },
      "default": null
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 256,
      "default": 64
    },
    "ready": {
      "type": "boolean",
      "default": false,
      "description": "When true, include only open, active Issues with no open or in-progress blockers"
    },
    "order": {
      "type": "string",
      "enum": [
        "asc",
        "desc"
      ],
      "default": "asc",
      "description": "Sort by display number; desc lists newest first. The cursor works in both orders"
    },
    "title_contains": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 128,
      "default": null,
      "description": "Case-insensitive substring filter over Issue titles; a bound worker needs this, labels or status (at least one)"
    },
    "fields": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "summary",
        "full",
        null
      ],
      "default": null,
      "description": "summary: display_number, id, row_version, title, status, labels, priority, created_at, updated_at (no body). full: whole records. Default: summary when title_contains is set, else full. Fetch one body with AgentGetIssue"
    }
  }
}"##;
const GET_ISSUE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "issue_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Issue UUID; set exactly one of issue_id and display_number"
    },
    "display_number": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"
    }
  }
}"##;
const UPDATE_ISSUE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "expected_row_version",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "issue_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Issue UUID; set exactly one of issue_id and display_number"
    },
    "display_number": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"
    },
    "expected_row_version": {
      "type": "integer",
      "minimum": 1
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    },
    "include_issue": {
      "type": "boolean",
      "default": false,
      "description": "Include the full Issue snapshot in the mutation receipt"
    },
    "title": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 512,
      "default": null
    },
    "body": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 65536,
      "default": null
    },
    "labels": {
      "type": [
        "array",
        "null"
      ],
      "maxItems": 64,
      "items": {
        "type": "string",
        "maxLength": 128
      },
      "default": null
    },
    "priority": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 4,
      "default": null
    },
    "clear_priority": {
      "type": "boolean",
      "default": false
    },
    "assignee": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 256,
      "default": null
    },
    "clear_assignee": {
      "type": "boolean",
      "default": false
    },
    "handoff": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 65536,
      "default": null,
      "description": "An append-only record on the Issue (up to 65536 bytes), published even when the body is at its ceiling: an Issue-bound worker's complete handoff, or a lead's or manager's note (#3052) that needs no body copy; set no other field. expected_row_version is not checked and a closed Issue accepts it. AgentGetIssue returns the newest records in full as handoffs."
    },
    "replace_body": {
      "type": "boolean",
      "default": false,
      "description": "Lead or manager: set true with body to replace the body on purpose. Without it a body that does not keep the end of the current body is refused body_would_drop_tail (#3052)."
    }
  }
}"##;
const UPDATE_ISSUE_STATUS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "status",
    "expected_row_version",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "issue_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Issue UUID; set exactly one of issue_id and display_number"
    },
    "display_number": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"
    },
    "status": {
      "type": "string",
      "enum": [
        "Open",
        "InProgress",
        "Closed",
        "Cancelled"
      ]
    },
    "expected_row_version": {
      "type": "integer",
      "minimum": 1
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    },
    "include_issue": {
      "type": "boolean",
      "default": false,
      "description": "Include the full Issue snapshot in the mutation receipt"
    },
    "note": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 2048,
      "default": null,
      "description": "Optional why: covered-by SHA, duplicate of #N, moot, landed SHA, or a reopen reason. Recorded on the status-change Issue event; AgentGetIssue returns it as status_note and AgentListIssueEvents as request.note"
    }
  }
}"##;
const ISSUE_CAS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "issue_id",
    "expected_row_version",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "issue_id": {
      "type": "string",
      "format": "uuid"
    },
    "expected_row_version": {
      "type": "integer",
      "minimum": 1
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    }
  }
}"##;
const LIST_ISSUE_EVENTS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "issue_id"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "issue_id": {
      "type": "string",
      "format": "uuid"
    },
    "after_sequence": {
      "type": "integer",
      "minimum": 0,
      "default": 0
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 256,
      "default": 64
    }
  }
}"##;

const CONTINUE_CHILD_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "x-exactly-one-of": [
    [
      "session_id",
      "target_session_id"
    ]
  ],
  "required": [
    "query",
    "expected_tip_session_id",
    "expected_event_sequence"
  ],
  "properties": {
    "session_id": {
      "type": "string",
      "format": "uuid",
      "description": "The child session to continue; the caller itself is refused"
    },
    "target_session_id": {
      "type": "string",
      "format": "uuid",
      "deprecated": true,
      "description": "Deprecated alias of session_id, accepted for one release (#2313); send session_id"
    },
    "query": {
      "type": "string",
      "minLength": 1,
      "maxLength": 262144,
      "description": "Continuation prompt delivered as the child's next turn"
    },
    "expected_tip_session_id": {
      "type": "string",
      "format": "uuid",
      "description": "Required staleness fence: the lineage tip the caller last observed"
    },
    "expected_event_sequence": {
      "type": "integer",
      "minimum": 0,
      "description": "Required staleness fence: MAX(sequence) of the tip's conversation events as last observed"
    },
    "expected_custody_generation": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Optional staleness fence: the sandbox custody generation as last observed; must match exactly including absence"
    }
  }
}"##;
const ARCHIVE_CHILD_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "x-exactly-one-of": [
    [
      "session_id",
      "target_session_id"
    ]
  ],
  "required": [
    "expected_tip_session_id",
    "expected_event_sequence"
  ],
  "properties": {
    "session_id": {
      "type": "string",
      "format": "uuid",
      "description": "Terminal child to archive; current Epic lead only"
    },
    "target_session_id": {
      "type": "string",
      "format": "uuid",
      "deprecated": true,
      "description": "Deprecated alias of session_id, accepted for one release (#2313); send session_id"
    },
    "expected_tip_session_id": {
      "type": "string",
      "format": "uuid",
      "description": "Required staleness fence: last observed lineage tip"
    },
    "expected_event_sequence": {
      "type": "integer",
      "minimum": 0,
      "description": "Required staleness fence: last observed tip event sequence"
    },
    "expected_custody_generation": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Optional staleness fence: last observed sandbox custody generation"
    }
  }
}"##;

const MANAGER_PROGRESS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "fields": {"type":"string", "enum":["compact","full"], "default":"compact", "description":"compact bounds titles and latest response excerpts; full restores detailed rows. Fences and paging are unchanged."},
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "after_epic_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "description": "Continue after next_after_epic_id from the previous page."
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 64,
      "default": 32
    }
  }
}"##;
const MANAGER_INBOX_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "include_sent": {
      "type": "boolean",
      "default": false,
      "description": "Include the manager seat's outbound sends for audits. Routine reads return incoming mail; request_id or fields:\"full\" also includes sent mail. notice_kind filters still apply."
    },
    "after_notice_sequence": {
      "type": "integer",
      "minimum": 0,
      "default": 0
    },
    "notice_kind": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "session_state",
        "message",
        "action_result",
        "operator_answer",
        "ledger_change",
        null
      ],
      "default": null,
      "description": "Filter notices by kind. Messages are returned only when omitted/null or set to message, including audit reads."
    },
    "settle_notice_ids": {
      "type": "array",
      "items": {
        "type": "string",
        "format": "uuid"
      },
      "maxItems": 32,
      "default": []
    },
    "read_notice_ids": {
      "type": "array",
      "items": {
        "type": "string",
        "format": "uuid"
      },
      "maxItems": 32,
      "default": [],
      "description": "Re-read these exact notices of your seat by id, settled or not, in the fields form chosen. A summary drain settles a notice but keeps its full payload: pass its notice_id here with fields:\"full\" to expand it. Read-only: settles nothing, drains no other notice and moves no cursor; an unknown, retired or foreign id refuses the call."
    },
    "fields": {
      "type": "string",
      "enum": [
        "compact",
        "full",
        "summary"
      ],
      "default": "compact",
      "description": "compact (default) omits repeated historical work_versions and carries work_versions_count; full restores the complete state payload and includes outbound manager sends. summary drops the state payload and returns a short state_summary string instead (a ledger_change's payload state and reason; a satellite_report's kind, peer and whole untrusted_text, up to the 1024 bytes AgentReportToHub accepts; an action_result's action_kind, state and outcome; otherwise the record_kind). Returned notices settle in every form. Use summary to drain a routine backlog."
    },
    "after_sequence": {
      "type": "integer",
      "minimum": 0,
      "default": 0
    },
    "limit": {
      "type": "integer",
      "minimum": 1,
      "maximum": 32,
      "default": 32
    },
    "request_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Optional recorded request to read within your live manager or feature-lead scope"
    }
  }
}"##;
const MANAGER_SEND_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "epic_id",
    "message",
    "idempotency_key"
  ],
  "properties": {
    "epic_id": {
      "type": "string",
      "format": "uuid",
      "description": "An Epic in your operator-appointed manager scope; the daemon resolves its current lead"
    },
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 8192,
      "description": "Nonblank request, at most 8192 UTF-8 bytes, without NUL"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"
    },
    "informational": {
      "type": "boolean",
      "default": false,
      "description": "Deliver auditable information without reserving a pending request slot or requiring a reply"
    }
  }
}"##;
const MANAGER_REPLY_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "request_id",
    "message",
    "idempotency_key"
  ],
  "properties": {
    "request_id": {
      "type": "string",
      "format": "uuid",
      "description": "Recorded request addressed to the Epic you currently lead"
    },
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 8192,
      "description": "Explicit reply with evidence or a blocker, at most 8192 UTF-8 bytes, without NUL"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"
    },
    "still_running": {
      "type": "boolean",
      "default": false,
      "description": "Keep the request active after this reply; omit or false to settle it"
    }
  }
}"##;
const MANAGER_NOTIFY_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "message",
    "idempotency_key"
  ],
  "properties": {
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 8192,
      "description": "Nonblank informational notice to your current appointed manager, at most 8192 UTF-8 bytes, without NUL; not a request, approval or acceptance"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"
    }
  }
}"##;

const MANAGER_WORK_VIEW_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "work_key": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 256,
      "default": null,
      "description": "Optional exact work key in your Epic"
    },
    "after_work_key": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 256,
      "default": null,
      "description": "Continue after next_after_work_key from the previous page"
    },
    "limit": {
      "type": "integer",
      "minimum": 1,
      "maximum": 32,
      "default": 32
    }
  }
}"##;
const MANAGER_DELEGATE_NODE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "expected_node_grant_version",
    "expected_parent_authority_epoch",
    "expected_parent_grant_version",
    "expected_parent_policy_version",
    "grant",
    "idempotency_key",
    "node_id",
    "parent_node_id",
    "policy",
    "seat_root_session_id",
    "selector"
  ],
  "properties": {
    "node_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid"
    },
    "parent_node_id": {
      "type": "string",
      "format": "uuid"
    },
    "seat_root_session_id": {
      "description": "A parentless Standard session or a Task directly under an Epic covered by selector, in the same project, without a predecessor or another active seat.",
      "type": "string",
      "format": "uuid"
    },
    "selector": {
      "type": "object"
    },
    "grant": {
      "type": "object",
      "properties": {
        "max_direct_reports": {"type": "integer", "minimum": 1, "maximum": 64}
      }
    },
    "policy": {
      "type": "object"
    },
    "expected_parent_grant_version": {
      "type": "integer",
      "minimum": 1
    },
    "expected_parent_policy_version": {
      "type": "integer",
      "minimum": 0
    },
    "expected_parent_authority_epoch": {
      "type": "integer",
      "minimum": 1
    },
    "expected_node_grant_version": {
      "type": "integer",
      "minimum": 0
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    }
  }
}"##;
const MANAGER_ESCALATE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "expected_source_authority_epoch",
    "expected_source_grant_version",
    "expected_target_authority_epoch",
    "expected_target_grant_version",
    "expected_target_session_id",
    "idempotency_key",
    "reason",
    "route",
    "subject_id"
  ],
  "properties": {
    "subject_id": {
      "type": "string",
      "format": "uuid"
    },
    "reason": {
      "type": "string",
      "minLength": 1,
      "maxLength": 8192
    },
    "route": {
      "type": "object"
    },
    "expected_source_authority_epoch": {
      "type": "integer",
      "minimum": 1
    },
    "expected_source_grant_version": {
      "type": "integer",
      "minimum": 1
    },
    "expected_target_authority_epoch": {
      "type": "integer",
      "minimum": 1
    },
    "expected_target_grant_version": {
      "type": "integer",
      "minimum": 1
    },
    "expected_target_session_id": {
      "type": "string",
      "format": "uuid"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    }
  }
}"##;
const MANAGER_LIST_ESCALATIONS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {}
}"##;
const MANAGER_RESOLVE_ESCALATION_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "escalation_id",
    "expected_target_authority_epoch",
    "expected_target_grant_version",
    "expected_target_session_id",
    "expected_version",
    "idempotency_key",
    "ruling"
  ],
  "properties": {
    "escalation_id": {
      "type": "string",
      "format": "uuid"
    },
    "expected_version": {
      "type": "integer",
      "minimum": 1
    },
    "expected_target_authority_epoch": {
      "type": "integer",
      "minimum": 1
    },
    "expected_target_grant_version": {
      "type": "integer",
      "minimum": 1
    },
    "expected_target_session_id": {
      "type": "string",
      "format": "uuid"
    },
    "ruling": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 8192
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128
    }
  }
}"##;

const GET_AUTHORITY_CATALOG_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "verb": {
      "type": [
        "string",
        "null"
      ],
      "default": null,
      "description": "Optional control name: an Agent* method, a native rsi_control_* tool, or its mcp__rsi-agent__ spelling. With verb the response is compact: just that control's detail (whether you may call it, parameter schema, one minimal valid example request, and its stable refusal codes with next steps); omit verb for the whole manual."
    },
    "fields": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "summary",
        "full",
        null
      ],
      "default": null,
      "description": "Whole-manual detail (ignored with verb). summary (default): roles, guidance and every permitted control by name with a one-line description, small enough to read inline. full: each control's complete description (tooling; roughly 40 KB larger for a manager). Schemas, examples and refusals are only ever behind verb."
    }
  }
}"##;

const SUBMIT_JOB_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "x-at-most-one-of": [
    [
      "source_session_id",
      "sandbox_session_id"
    ]
  ],
  "required": [
    "kind",
    "params"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "source_session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "landing and cloud_gate only: run in this terminal, in-reach session's sandbox"
    },
    "sandbox_session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "deprecated": true,
      "description": "Deprecated alias of source_session_id, accepted for one release (#2313); send source_session_id"
    },
    "kind": {
      "type": "string",
      "enum": [
        "test",
        "build",
        "landing",
        "cloud_gate",
        "cloud_sweep",
        "bench"
      ]
    },
    "params": {
      "type": "object",
      "description": "Profile verification (#413): non-Rust scoped_test resolves .rsi/project.toml, legacy .rsi/jobs.toml or a literal local make/just test target; all verification commands must complete successfully. result.profile_verification is daemon-written source/profile/tool evidence, never parsed from project output, with generic_command_exit warning applicability and completed command counts (not individual test counts). Rust projects retain scoped-test baseline/warning gates. test: {recipe: declared name in the resolved project profile or legacy .rsi/jobs.toml (version 1; recipes.NAME: runner just|make, target, timeout_minutes cap 5-180, cpu_quota_percent 1-1600); uses local justfile/Makefile; no arguments; request timeout must fit the declaration; scoped-test/scoped-test-tmpfs names and targets are retired (job_recipe_use_scoped_test); use scoped_test instead} or {scoped_test:{base,head?,rolling_tool?}: run the rolling-tip scripts/scoped-test --base <ref> [--head <ref>] in your sandbox (candidate tool changes run their own copy; rolling_tool:true forces the rolling copy for trusted verification and refuses scoped_test_rolling_tool_unavailable if it cannot run; result.scoped_tool holds daemon-written source (rolling|candidate|sandbox), path, rev and file blobs independently of receipt.tool; candidate_tool_untrusted requires a rolling-tool rerun at the same head; bare branch, origin/<branch> or 40-hex refs only, no other arguments; submit with wake none, arm ONE AgentScheduleWake mode when, end your turn; result.exit_code and the job log_path are set, result.receipt is the scoped-test receipt {ok,exit_code,base,head,filters,log_dir,packages[{package,status,completed}]}, refusal scoped_test_receipt_missing when none was printed)} or {shard,filterset?: one or more nextest atoms test(NAME), test(=NAME) or test(/REGEX/) joined by ` | `, at most 32 atoms and 1024 bytes} or {package,filters[],lib_only?,test_target?:integration test name (ASCII letters, digits, underscore or hyphen; no leading hyphen, path or shell syntax; passes cargo --test <name>; requires exact:true and nonempty filters; mutually exclusive with lib_only), bin?:binary target name (same token rules; passes cargo --bin <name>, e.g. rsid rsi-rolling-land unit tests; requires exact:true and nonempty filters; mutually exclusive with lib_only and test_target),features?:[dev feature of the package from the daemon allowlist, today rsid e2e-seed; passes cargo --features; package only; any other feature is refused job_invalid_params naming the allowed ones, like a check or build job; still bounded by rsi-build.slice, the sandbox target and the job timeout],exact?:boolean (default false; true passes libtest --exact for whole test names; package only; empty filters still run every test; with filters, a run whose whole log proves libtest executed no test, 0 passed and 0 failed in every test binary, settles failed refusal job_filter_matched_no_tests with exit_code 0 kept, so a mistyped name is never green; log evidence you cannot read, such as a custom harness, keeps the ordinary result; optional early feedback for a new runtime or schema assertion, never acceptance: only the committed scoped_test is)} or {package,filters[],exact:true,lib_only|test_target|bin,at_commit: 40-hex commit id of this repository: run those exact tests at that past commit in a scratch worktree the job creates and removes, so your sandbox, branch and index are untouched (confirm which commit broke a test, or run a reviewed source's new tests from a sandbox at rolling); open to any session with a sandbox; never acceptance evidence, only the committed scoped_test is; refusal job_invalid_params names the missing field, and a commit the repository lacks exits 2 with `not in this repository`} or {candidate_receipt: branch or 40-hex sha; manager/Epic lead only; result.receipt is the typed candidate receipt}; every package, shard or scoped_test job (not recipe or candidate_receipt) and every build job also takes cpu_quota_percent? (100-1600, percent of one CPU; omitted keeps the daemon-derived budget; the daemon applies the smaller of it and its own ceiling, so a request only lowers the operator/hub budget, before the unit starts: the unit CPUQuota, a scoped_test --cpu-quota, and the build and test thread counts, a scoped_test receipt records the effective cpu_quota); every test job also takes timeout_minutes? (a worker omits it: the default fits an exact-name package run, and a larger value from anyone but the manager or an Epic lead is refused job_timeout_not_authorized; 5-180; default the operator job_test_timeout_mins, 20, except a scoped_test job whose default is 60 or job_test_timeout_mins if larger (its tool budgets each package up to the timeout minus 2 minutes, total minus 1; the stamped params and the receipt's job_timeout_minutes show the effective timeout); above the default needs the manager or an Epic lead, except qa_lane:{sha:40-lowercase-hex} on a known shard: current live unsuperseded AgentManagerLaunchIssueWorker binding with explicit manager qa_lane:true delegation or manager/Epic lead, timeout 5-90, default capped at 90, SHA labels submission-time HEAD only; no dirtiness check or exact-tested-bytes proof; use a manager pinned detached worktree plus worktree on an ordinary test for an exact pin, at most two outstanding QA lanes per owner (queued or running); QA never supports recipe, scoped_test, package or candidate_receipt; past timeout the unit is stopped and the job fails job_timed_out); build: {command:check|build, one of package:<workspace-member-name> or workspace:true, all_targets?,release?,features?:[FEATURE,...] (package only, never with release; only the dev features allowed for that package: rsid allows e2e-seed, which compiles the isolated-state seeder binary rsi-e2e-seed; any other feature is refused job_invalid_params naming the allowed ones),bins?:[BIN,...] (package only; passes --bin NAME, at most 8; build rsid with features:[e2e-seed] and bins:[rsid,rsi-e2e-seed] for the desktop e2e smoke),cpu_quota_percent?,sha?:40-lowercase-hex (pins the commit the build compiles; refused job_source_sha_mismatch when the job worktree HEAD differs at submit)} (hub #2954: every build job compiles exactly one commit; the daemon records the job worktree HEAD and whether it has uncommitted changes at submit and rechecks at start, while it runs (slot grant included) and at finish; a moved HEAD, or a clean tree that became dirty, stops the unit and settles it failed refusal job_source_moved naming both SHAs; result.source_head and result.source_dirty carry the HEAD and dirtiness observed at the end so a deploy can compare them with the SHA it installs; a dirty tree at submit is built as is and only a HEAD move fails it) or {command:test_no_run (#2692), package, no_default_features?, then either features?:[test-shard-NAME,...] (one build of the lib test harness, cargo test --lib --no-run) or shards?:[NAME,...] (a shard-feature sweep: each shard built with --no-default-features --features test-shard-NAME one after another, the package cleaned after each so the target holds one harness at a time; at most 32); runs in rsi-build.slice under the hub CPU budget; result.receipt is the SHARD_SWEEP_RECEIPT {ok,package,shards[{shard,features,exit_code,warnings{package:count},warning_keys},...],warnings:<warning-baseline gate verdict>}, settled failed when any shard build fails or the baseline gate reports new_warnings; this is the way to verify shard features, never a self-launched cargo unit}; or build {command:build,release:true,sha:40-lowercase-hex,release_members:[{package,bins},...]}: up to 16 distinct packages and 64 globally distinct bins, each package compiled in a separate Cargo invocation under one build slot, daemon-selected target and clean pinned source fence; omit single-build flags; first failure stops the job; successful result.release_members proves complete staging coverage; package names are checked against cargo metadata at submission and unknown names are refused as invalid_input field=params.package; landing/cloud_gate: {accepted:40-hex sha,test_filters?:[PACKAGE=FILTER]}; cloud_sweep: {sha:40-hex current rolling tip, lane?:build or qa (default qa, the full-library QA sweep; build runs cargo build --workspace --locked plus a small test set under the same spend net, no Python)}; bench (#3181, manager or Epic lead only): {config:eval/bench/tb2/runs/NAME.toml committed and clean at HEAD,budget_usd:number}: one pinned Terminal-Bench run on the laptop; needs bench.enabled, the config host in bench.docker_hosts, allowed_launches, an armed OpenRouter spend cap with headroom for budget_usd (a config that launches no model runs with the cap off), pin_check and the laptop preflight; result.bench is the run summary and record_path the BenchRunRecord. Typed fields only; no command line."
    },
    "name": {
      "type": "string",
      "minLength": 1,
      "maxLength": 80
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Optional replay key"
    },
    "worktree": {
      "type": "string",
      "description": "Appointed manager only: another worktree of your own repository to run in; default is your sandbox"
    },
    "wake": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "owner",
        "none",
        null
      ],
      "default": "owner",
      "description": "owner (default): one resume wake to you when this job settles. none: no per-job wake; the job still settles and AgentGetJob/AgentListJobs return its result. Submit a batch with none, then arm one AgentScheduleWake mode when with when.jobs_terminal so you are woken once"
    }
  }
}"##;

static SUBMIT_JOB_DETAILED_SCHEMA: LazyLock<String> = LazyLock::new(|| {
    let mut schema: Value = serde_json::from_str(SUBMIT_JOB_SCHEMA).expect("job schema");
    diagnostics::add_job_schemas(&mut schema);
    schema.to_string()
});

const GET_JOB_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "job_id"
  ],
  "properties": {
    "job_id": {
      "type": "string",
      "pattern": "^[0-9a-fA-F-]{8,36}$",
      "description": "A job in your current read scope: its UUID, or the unique prefix of at least 8 hex characters that notices print. A copied UUID alone grants no access."
    },
    "fields": {
      "type": "string",
      "enum": ["compact", "full"],
      "default": "full",
      "description": "full (default): the whole stored job. compact (hub #3852): a bounded view of at most about 20k characters for a finished scoped_test (verdict, base, head, merges_cleanly_onto_rolling, new_failures, baseline_reds, failures and failing_tests capped with *_omitted counts, per-package package/status/completed, the warnings verdict, a short log tail; params, cwd and tool blobs are left out and result.dropped names the result keys cut). Use compact after a scoped_test; full when you need the complete receipt."
    }
  }
}"##;

const CANCEL_JOB_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "job_id"
  ],
  "properties": {
    "job_id": {
      "type": "string",
      "format": "uuid"
    }
  }
}"##;

const LIST_JOBS_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "limit": {
      "type": "integer",
      "minimum": 1,
      "maximum": 100
    },
    "fields": {
      "type": "string",
      "enum": ["compact", "full"],
      "default": "compact",
      "description": "compact (default, hub #3889): one bounded row per job (id, name, kind, state, exit_code, refusal, receipt_head when present, held, created_at/started_at/finished_at, log_path, wake) and no result payload; read a job whole with AgentGetJob. full: every job with its complete typed result (a scoped_test receipt is tens of thousands of characters)."
    }
  }
}"##;

const REPORT_TO_HUB_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "kind",
    "text"
  ],
  "properties": {
    "kind": {
      "type": "string",
      "enum": [
        "enqueue",
        "deploy_ready",
        "result"
      ],
      "description": "Report line type (ENQUEUE, DEPLOY-READY, RESULT)"
    },
    "binaries_dir": {
      "type": "string",
      "description": "Required for deploy_ready only: existing directory under the satellite staging root. The satellite expands ~/ using its own home and sends the resolved absolute path to the hub."
    },
    "text": {
      "type": "string",
      "minLength": 1,
      "maxLength": 1024,
      "description": "Short report line, at most 1024 UTF-8 bytes; informational and untrusted on the hub. The hub manager reads the whole text in every AgentManagerInbox form, the summary form included (#195)"
    }
  }
}"##;
const SEND_SATELLITE_MESSAGE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "peer_id",
    "remote_session_id",
    "message",
    "idempotency_key"
  ],
  "properties": {
    "peer_id": {
      "type": "string",
      "description": "Operator-registered satellite peer id"
    },
    "remote_session_id": {
      "type": "string",
      "description": "Target on the satellite: a session id inside the operator-declared scope, a declared project id (delivered to that project's current appointed manager, resolved by the satellite), or, with scope_project_id, a session in that project's manager tree"
    },
    "scope_project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Optional declared project that authorizes remote_session_id when it is a session in that project's current manager's tree (the manager itself, a descendant, or a rotation predecessor); the satellite resolves the seat live and refuses a target outside it"
    },
    "message": {
      "type": "string",
      "minLength": 1,
      "maxLength": 16384,
      "description": "Message text; queued until the remote session is idle"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL"
    },
    "expires_at": {
      "type": "string",
      "description": "Optional RFC3339 expiry, at most 24 hours ahead; default 30 minutes"
    },
    "issue_refs": {
      "type": "array",
      "maxItems": 8,
      "uniqueItems": true,
      "items": {
        "type": "integer",
        "minimum": 1
      },
      "default": [],
      "description": "Hub Issue display numbers of YOUR OWN project, appended whole (title, status, complete body and acceptance) to the delivered message, read-only, in the order named. The project is never named: only Issues your own AgentGetIssue can read expand. message plus every Issue must fit maxLength 16384 or the call is refused, never truncated. The snapshot is taken once at acceptance; a replay returns the first receipt"
    }
  }
}"##;
const ENQUEUE_LANDING_SOURCE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "source_commit",
    "idempotency_key"
  ],
  "properties": {
    "issue": {
      "type": ["integer", "null"],
      "minimum": 1,
      "default": null,
      "description": "Issue display number in the source project, stored on the entry and returned by AgentListRollingQueue. AgentTopologyExecute on that Issue warns about queued/admitted/gating entries; this is tracking metadata, separate from fixes_issue test claims"
    },
    "supersedes": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Replace a queued entry id in your cancellation scope (including a predecessor manager seat) with a descendant source, preserving its queue position and single wake. An admitted/gating entry is queue_entry_in_flight: enqueue normally without supersedes"
    },
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "source_session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Land from this in-reach session's sandbox instead of your own"
    },
    "source_commit": {
      "type": "string",
      "pattern": "^[0-9a-f]{40}$",
      "description": "Full lowercase 40-hex accepted source commit reachable from your sandbox"
    },
    "test_filters": {
      "type": "array",
      "maxItems": 32,
      "items": {
        "type": "string",
        "minLength": 3,
        "maxLength": 256,
        "description": "PACKAGE=FILTER, at most 256 UTF-8 bytes; the gate is a compile check plus the named tests"
      },
      "default": [],
      "description": "Explicit filters, at most 32 of at most 256 bytes each (queue_filter_invalid otherwise). Prefer derive_filters; never copy, prune or split check-touched-shards output by hand. With derive_filters these are extra filters: the queue gates the derived set plus these"
    },
    "derive_filters": {
      "type": "boolean",
      "default": false,
      "description": "Let the queue derive the filters: the daemon runs scripts/check-touched-shards on the source diff against the rolling tip, dedupes focused rsid/rsid-store filters naming the same expression across shards, and splits any filter over 256 bytes into equivalent filters. A derived set over 32 filters is not refused. Combines with test_filters (the derived set plus those). One call is enough: a derivation still running at the call's budget records the entry queued with filters_state deriving (the lander admits it only once its filters land) and returns it; never sleep or call again. A derivation that exceeds its budget (it times out, or the diff changes too many files to trace in time) is not refused: the entry is kept with filters_state lander_selects and only the filters you named, and the lander selects the tests itself (its bounded closure selection plus the automatic full-library QA)"
    },
    "expedite": {
      "type": ["object", "null"],
      "additionalProperties": false,
      "required": ["reason"],
      "default": null,
      "properties": {
        "reason": {
          "type": "string",
          "minLength": 1,
          "maxLength": 256,
          "description": "Why this landing jumps the queue, 1 to 256 UTF-8 bytes, no control character; recorded on the entry and shown by AgentListRollingQueue"
        }
      },
      "description": "Appointed manager only (an Epic lead is refused queue_expedite_not_authorized): admit this entry into the next batch ahead of ordinary queued entries, in enqueue order among expedited ones. A gating batch is never preempted and the batch keeps every other policy (one change per gate, filters, migrations). At most 2 live (queued, admitted or gating) expedited entries per project; one more is refused queue_expedite_cap_reached, which lists them. Use it for an incident or a red-QA fix that cannot wait behind the queue, never to reorder routine work. A fixes_issue naming an open Issue labelled qa expedites with no flag (past the cap it stays ordinary and the receipt carries expedite_warning)"
    },
    "fixes_issue": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Display number of the red Issue this landing claims to fix: the queue adds the test names its body lists on Failing tests: lines (the qa Issue format) or on the gate base-red Issue's Test: line to fixes_tests (queue_fixes_issue_not_found, queue_fixes_issue_lists_no_failing_tests); the combined claims are capped at 64 (queue_fixes_too_many names the count)"
    },
    "fixes_tests": {
      "type": "array",
      "maxItems": 64,
      "items": {
        "type": "string",
        "minLength": 1,
        "maxLength": 512,
        "pattern": "^[A-Za-z0-9_:.][A-Za-z0-9_:.-]*$",
        "description": "A test name as the gate and the QA run report it, for example module::tests::name"
      },
      "default": [],
      "description": "Tests this landing claims to turn green. The gate must run each on the candidate and see it pass, or the landing is refused claimed_fix_still_red (even when the base is red on it too); a test the gate's selection does not run is refused the same way, but the queue adds the filter that selects the tests a fixes_issue Issue names itself, from its Shard command: line (cargo test -p PKG --test|--bin NAME selects PKG=test:NAME or PKG=bin:NAME) and from its Failing outcomes: line (FAIL PKG PATH selects PKG=test(=PATH)); for a fixes_tests name outside such an Issue, add a test_filters entry that selects it. A Python unittest id (scripts.tests.<module>.<Class>.<method> or tools.tests.<module>...) needs no filter: the gate runs that test module itself. Combines with derive_filters or test_filters"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL"
    }
  }
}"##;

const LIST_ROLLING_QUEUE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "fields": {
      "type": ["string", "null"],
      "enum": ["summary", "full", null],
      "default": null,
      "description": "Default summary: live entries plus refused/failed entries finished in the last hour, with compact outcomes; full restores all history and existing detail. An explicit state also restores complete output"
    },
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Narrow to one project inside your scope (a portfolio seat: one covered project); omit for every entry in your scope"
    },
    "state": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "queued",
        "admitted",
        "gating",
        "published",
        "refused",
        "failed",
        "superseded",
        null
      ],
      "default": null,
      "description": "Only entries in this state, with complete history and existing outcome detail"
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 100,
      "default": null,
      "description": "Page size, 100 at most (default 50)"
    },
    "before_sequence": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "Page cursor: only entries older than this sequence (the previous page's next_before_sequence)"
    }
  }
}"##;

const CANCEL_ROLLING_QUEUE_ENTRY_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "entry_id"
  ],
  "properties": {
    "entry_id": {
      "type": "string",
      "format": "uuid",
      "description": "Queue entry id from AgentListRollingQueue or the enqueue receipt; only a still-queued entry can be cancelled"
    },
    "reason": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 256,
      "default": null,
      "description": "Why, recorded in the entry's outcome detail (at most 256 bytes, no control characters)"
    }
  }
}"##;

const CHECK_LANDING_CANDIDATE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "source_commit"
  ],
  "properties": {
    "job_id": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^[0-9a-f-]{8,36}$",
      "default": null,
      "description": "The verification job: a full id, or a prefix of at least 8 hex characters that is unique inside your scope (candidate_job_id_ambiguous otherwise). Omit it and name source_session_id to check that worker's newest scoped_test job"
    },
    "source_session_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "The worker session that ran the verification. Without job_id, the newest scoped_test job it owns whose receipt head is source_commit is checked, else its newest scoped_test job. With job_id, the job must belong to this session"
    },
    "source_commit": {
      "type": "string",
      "pattern": "^[0-9a-f]{40}$",
      "description": "Full lowercase 40-hex commit you would pass to AgentEnqueueLandingSource"
    }
  }
}"##;

const TOPOLOGY_UPSERT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "name",
    "definition",
    "scope",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "name": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Topology name, unique per owner"
    },
    "definition": {
      "type": "object",
      "required": [
        "nodes",
        "edges"
      ],
      "properties": {
        "nodes": {
          "type": "array",
          "items": {
            "type": "object"
          }
        },
        "edges": {
          "type": "array",
          "items": {
            "type": "object"
          }
        },
        "until": {
          "type": [
            "object",
            "null"
          ]
        }
      },
      "description": "TopologyDefinition: nodes (id, kind, label, prereqs, params incl. typed step, custody, explicit provider/model/effort), edges, until; the daemon validates it and returns diagnostics"
    },
    "scope": {
      "type": "string",
      "enum": [
        "epic",
        "manager"
      ],
      "description": "epic: owned by one Epic (leads may only use this); manager: owned by the current manager"
    },
    "epic_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Target Epic for a manager epic-scoped upsert; a lead's Epic is daemon-derived and, if given, must match"
    },
    "expected_revision": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "default": null,
      "description": "CAS fence to revise an existing topology; omit to create"
    },
    "validate_only": {
      "type": "boolean",
      "default": false,
      "description": "Report diagnostics without writing"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"
    }
  }
}"##;
const TOPOLOGY_LIST_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "scope": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "epic",
        "manager",
        null
      ],
      "default": null
    },
    "epic_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Narrow to one Epic in your scope"
    },
    "include_executions": {
      "type": "boolean",
      "default": false,
      "description": "Also list the scope's live executions (accepted, running, cancelling, blocked); the default list is live-only"
    },
    "include_settled": {
      "type": "boolean",
      "default": false,
      "description": "Also list settled executions (succeeded, failed, cancelled): the run history. Implies include_executions. Live executions come first, then settled newest first, at most 32 in all"
    },
    "cursor": {
      "type": [
        "string",
        "null"
      ],
      "maxLength": 128,
      "default": null,
      "description": "next_cursor from the previous page"
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 32,
      "default": null
    }
  }
}"##;
const TOPOLOGY_EXECUTE_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "topology_id",
    "expected_digest",
    "epic_id",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "topology_id": {
      "type": "string",
      "format": "uuid"
    },
    "expected_digest": {
      "type": "string",
      "pattern": "^sha256:[0-9a-f]{64}$",
      "description": "definition_digest returned by list or upsert"
    },
    "epic_id": {
      "type": "string",
      "format": "uuid",
      "description": "Epic the execution runs under; must be in your scope"
    },
    "inputs": {
      "type": [
        "object",
        "null"
      ],
      "default": null
    },
    "on_call": {
      "description": "Seat that answers node questions: {\"kind\":\"project_manager\"} (default: the project manager, else the covering portfolio seat) or {\"kind\":\"portfolio\",\"node_id\":uuid}",
      "default": null,
      "oneOf": [
        {
          "type": "null"
        },
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "kind"
          ],
          "properties": {
            "kind": {
              "const": "project_manager"
            }
          }
        },
        {
          "type": "object",
          "additionalProperties": false,
          "required": [
            "kind",
            "node_id"
          ],
          "properties": {
            "kind": {
              "const": "portfolio"
            },
            "node_id": {
              "type": "string",
              "format": "uuid"
            }
          }
        }
      ]
    },
    "base_commit": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^[0-9a-fA-F]{40}$",
      "default": null,
      "description": "Full 40-hex base; omitted resolves origin/rolling once at acceptance"
    },
    "supersedes_execution": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "One Issue, one live run (#2427): when inputs.issue already has a live execution the request is refused topology_issue_already_live naming it; pass that execution's id here to interrupt it and start this one instead"
    },
    "wake": {
      "type": [
        "string",
        "null"
      ],
      "enum": [
        "owner",
        "none",
        null
      ],
      "default": "owner",
      "description": "owner (default): the daemon arms ONE topology_execution_settled resume wake to you when this execution settles (succeeded, failed, cancelled or blocked), like the outcome wake of AgentEnqueueLandingSource; the receipt's settle_wake_armed says so, and settle_wake_refusal carries the refusal code when the arm was refused (settle wakes have their own per-session cap, apart from other when wakes). An idempotent replay arms no second wake, and nothing is armed when you already own a settle wake for this execution (an owner keeps one enabled settle wake per execution). none: arm nothing (you arm your own AgentScheduleWake mode when with when.topology_execution_settled, e.g. one wake for several executions)"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL; a replay returns deduplicated=true"
    }
  }
}"##;
const TOPOLOGY_GET_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "topology_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Select by id; give exactly one of topology_id and name"
    },
    "name": {
      "type": [
        "string",
        "null"
      ],
      "minLength": 1,
      "maxLength": 128,
      "default": null,
      "description": "Select by name (names are unique across the daemon); give exactly one of topology_id and name"
    }
  }
}"##;
const TOPOLOGY_GET_EXECUTION_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "execution_id"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "execution_id": {
      "type": "string",
      "format": "uuid"
    },
    "after_sequence": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 0,
      "default": null,
      "description": "next_sequence from the previous page"
    },
    "limit": {
      "type": [
        "integer",
        "null"
      ],
      "minimum": 1,
      "maximum": 64,
      "default": null
    }
  }
}"##;
const TOPOLOGY_INTERRUPT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "execution_id",
    "expected_row_version",
    "idempotency_key"
  ],
  "properties": {
    "project_id": {
      "type": [
        "string",
        "null"
      ],
      "format": "uuid",
      "default": null,
      "description": "Global manager only: the granted project to act in; omit for your own project"
    },
    "execution_id": {
      "type": "string",
      "format": "uuid"
    },
    "expected_row_version": {
      "type": "integer",
      "minimum": 1,
      "description": "row_version observed via get_execution; refresh after stale_version"
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL"
    }
  }
}"##;
const TOPOLOGY_RESOLVE_ATTEMPT_SCHEMA: &str = r##"{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "execution_id",
    "attempt_id",
    "action",
    "expected_row_version",
    "idempotency_key"
  ],
  "properties": {
    "execution_id": {
      "type": "string",
      "format": "uuid"
    },
    "attempt_id": {
      "type": "string",
      "format": "uuid",
      "description": "Attempt blocked on preserved work"
    },
    "action": {
      "type": "string",
      "enum": [
        "inspect",
        "accept",
        "retry",
        "discard"
      ],
      "description": "discard is manager (Automation) only; a lead is refused"
    },
    "expected_row_version": {
      "type": "integer",
      "minimum": 1
    },
    "idempotency_key": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Replay key, at most 128 UTF-8 bytes, without NUL"
    },
    "confirm_preserved_commit": {
      "type": [
        "string",
        "null"
      ],
      "pattern": "^[0-9a-fA-F]{40}$",
      "default": null,
      "description": "Full 40-hex preserved_commit; required iff action=discard, refused otherwise"
    }
  }
}"##;

static AGENT_CONTROL_CATALOG_V1: LazyLock<[AgentControlDescriptorV1; 81]> = LazyLock::new(|| {
    [
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetAuthorityCatalog,
            method: "AgentGetAuthorityCatalog",
            description: "Your operator's manual: your current roles, the operating rules for those roles, and exactly the controls you may call now. The default answer lists each control with a one-line description; pass verb for one control only (its parameter schema, a minimal valid example and its refusal codes, no full list or guidance repeated), or fields \"full\" for complete descriptions. Read-only and open to every session; call it first and again after a role change or an authority refusal.",
            parameters_json: GET_AUTHORITY_CATALOG_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlAuthorityCatalog),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SpawnChild,
            method: "AgentSpawnChild",
            description: "Spawn a child agent session under the caller (caller must lead its owning Epic; else rejected NotLead).",
            parameters_json: SPAWN_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlSpawn),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReserveSuccessor,
            method: "AgentReserveSuccessor",
            description: "Reserve one daemon-authored same-Epic master successor; exact retries return the original candidate and authority transfers only after establishment.",
            parameters_json: RESERVE_SUCCESSOR_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlReserveSuccessor),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetProgress,
            method: "AgentGetProgress",
            description: "Read one bounded durable progress snapshot for the caller's child cohort or current manager's live scope.",
            parameters_json: PROGRESS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlProgress),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SendMessage,
            method: "AgentSendMessage",
            description: "Queue durable mail for your own reserved/direct child, a child of an Epic you lead, or a scoped leaf with manager SessionControl Execute authority; queued means accepted, not delivered, and never interrupts a running turn.",
            parameters_json: SEND_MESSAGE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlSendMessage),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetStatus,
            method: "AgentGetStatus",
            description: "Report status of the caller, its children, or a session in the current manager's live scope.",
            parameters_json: STATUS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlStatus),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::Halt,
            method: "AgentHalt",
            description: "Halt a child or a scoped leaf with manager SessionControl Execute authority. Halting another session atomically records halt intent and disables its own unprotected resume/when wakes (disabled_by_halt in AgentListWakes with include_disabled); self-halt preserves wakes. Detached jobs keep running; cancel eligible jobs separately with AgentCancelJob. A later explicit AgentContinueChild still works.",
            parameters_json: TARGET_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlHalt),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ContinueChild,
            method: "AgentContinueChild",
            description: "Continue an exact child or a scoped leaf with manager SessionControl Execute authority; checks the observed continuation cursor as an optimistic staleness fence and refuses a stale, self-targeted, or non-continuable provider target. Continuing a running child interrupts its active turn. This verb does not deduplicate delivery.",
            parameters_json: CONTINUE_CHILD_SCHEMA,
            // RPC-only in slice 1. The native in-process tools are constructed
            // with an `AgentControlHandle` alone, but the continuation engine hangs
            // off `SessionManager`; advertising a `rsi_control_continue_child` name
            // that resolves to no registered tool would be worse than declaring the
            // gap. Tracked as a follow-up.
            native_tool: Some(NativeAgentControlToolV1::RsiControlContinueChild),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ArchiveChild,
            method: "AgentArchiveChild",
            description: "Archive a terminal child of the Epic you currently lead using the observed continuation cursor. Current Epic lead only; RPC-only.",
            parameters_json: ARCHIVE_CHILD_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ScheduleWake,
            method: "AgentScheduleWake",
            description: "Schedule a future wake/callback; explicit mode is required: fresh, resume, on_terminal, program_guard, or when (use resume for same-session continuation; when is a daemon-evaluated predicate wake: jobs_terminal, sha_on_rolling, shas_on_rolling (all listed commits; with fire each, once per landing), issue_closed, todo_closed (an operator to-do item done or dismissed) or topology_execution_settled (a topology execution succeeded, failed, cancelled or blocked) or settlement_run_settled (a source-worktree settlement run finished) or health_anomaly (a manager seat only: the daemon checks its health conditions every minute with no model turn and resumes you once, naming each), one resume wake, optional timeout_seconds). The current manager may watch subjects in its live scope.",
            parameters_json: WAKE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::ScheduleWake),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CancelWake,
            method: "AgentCancelWake",
            description: "Disable your own scheduled wake(s) by job_id or name so a stale safety net cannot block succession, pause or lead replacement; refuses other sessions' jobs and the daemon-owned program guard. Rows are disabled, not deleted. RPC-only.",
            parameters_json: CANCEL_WAKE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlCancelWake),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListWakes,
            method: "AgentListWakes",
            description: "List your own scheduled wakes (job_id, name, mode, watch_session_id, the when predicate and deadline of a mode-when wake, next_fire_at, enabled, created_at, protected, and pending_message when a delivered report or notice is waiting inside the wake), bounded by limit (default 64, max 256); include_disabled adds cancelled or fired history. After a seat change this is how the successor confirms which wakes carried over. Your own wakes by default; `session_id` reads a live worker's wakes (read-only) when AgentWorkerRoster lists that worker for your current scope, otherwise `list_wakes_target_not_authorized`.",
            parameters_json: LIST_WAKES_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListWakes),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CreateIssue,
            method: "AgentCreateIssue",
            description: "Create an attributed durable issue follow-up; use --params @file for multiline bodies and never supply creator identity.",
            parameters_json: CREATE_ISSUE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlCreateIssue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListIssues,
            method: "AgentListIssues",
            description: "List a bounded page of project Issues. `fields` is `summary` (display_number, id, row_version, title, status, labels, priority, created_at, updated_at; no body) or `full` (whole records); omitted, a title_contains search returns summary rows and any other list keeps full records, so dedupe searches stay small (read one body with AgentGetIssue). A live Issue-bound worker must set at least one of title_contains, labels or status and omit project_id; it always receives summary rows from its bound project for deduplication. A live topology session node follows the same rule for its execution's project. Filters: title_contains (case-insensitive), labels (all exact matches), status, ready, archive, order and cursor.",
            parameters_json: LIST_ISSUES_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListIssues),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetIssue,
            method: "AgentGetIssue",
            description: "Read one Issue by issue_id or display_number (exactly one) with up to 256 blocked_by and blocks entries (each list has a *_truncated flag) in the project owned by the Epic you currently lead, or by an appointed manager with issue-coordinate authority. A worker launched for one Issue (AgentManagerLaunchIssueWorker) reads, while that binding is live, its own Issue, its review Issues and the same-project Issues it references with a `#N` or `#A-#B` selector in that Issue's body or handoffs or in the manager's admitted request.brief field, or that the launch attached (`related`). The whole compiled query is not scanned: references only in daemon-copied records grant no additional live Issue reads. review_of alone grants no reverse live read of the reviewed Issue; explicit selection can independently grant it. Reads are read-only, and a foreign project, a missing number or an unreferenced Issue is refused agent_issue_authority_denied.",
            parameters_json: GET_ISSUE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGetIssue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::UpdateIssue,
            method: "AgentUpdateIssue",
            description: "CAS-update active Issue content (target by issue_id or display_number, exactly one) as the current owning-Epic lead or manager with issue-coordinate authority. To add a note without resending the body, send handoff alone (a lead or manager appends a record; closed Issues accept it); a body that drops the end of the current body is refused body_would_drop_tail unless replace_body is true. A worker launched for one Issue (AgentManagerLaunchIssueWorker) may, while that binding is live, publish to that Issue only: send just handoff (your complete handoff, stored as its own record; no body copy, any positive expected_row_version) or, as the non-default alternative, body holding the current body unchanged as its prefix plus your section; any other field, Issue or status change is refused.",
            parameters_json: UPDATE_ISSUE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlUpdateIssue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::UpdateIssueStatus,
            method: "AgentUpdateIssueStatus",
            description: "CAS-update one Issue lifecycle status (target by issue_id or display_number, exactly one) as the current owning-Epic lead or manager with issue-coordinate authority.",
            parameters_json: UPDATE_ISSUE_STATUS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlUpdateIssueStatus),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ArchiveIssue,
            method: "AgentArchiveIssue",
            description: "Archive one terminal Issue with row-version CAS as the current owning-Epic lead or manager with issue-coordinate authority.",
            parameters_json: ISSUE_CAS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlArchiveIssue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::RestoreIssue,
            method: "AgentRestoreIssue",
            description: "Restore one archived Issue with row-version CAS as the current owning-Epic lead or manager with issue-coordinate authority.",
            parameters_json: ISSUE_CAS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlRestoreIssue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListIssueEvents,
            method: "AgentListIssueEvents",
            description: "Read bounded immutable Issue audit history as the current owning-Epic lead or manager with issue-coordinate authority.",
            parameters_json: LIST_ISSUE_EVENTS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListIssueEvents),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerProgress,
            method: "AgentManagerProgress",
            description: "Read bounded progress for your operator-appointed manager scope, including evidence and unanswered requests.",
            parameters_json: MANAGER_PROGRESS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerProgress),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerInbox,
            method: "AgentManagerInbox",
            description: "Retrieve incoming manager mail. Leads read fields:\"full\" for full bodies in notices[].state.message as well as messages[].message, even when historical mail leaves a message page empty; pass next_after_sequence as after_sequence to continue. include_sent:true, request_id or fields:\"full\" also returns outbound sends for audits. A notice_kind other than message suppresses messages. Page notices with after_notice_sequence and notice_kind, or settle_notice_ids belonging to your seat; fields:\"summary\" returns each notice without its state payload (a short state_summary instead) to drain a routine backlog cheaply. Returned notices settle automatically and stay retained: read_notice_ids:[<notice_id>], fields:\"full\" re-reads settled notices with their full payload, including live lead mail bodies; retrieval does not reply, answer a decision, clear a question, grant approval, or prove provider acceptance.",
            parameters_json: MANAGER_INBOX_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerInbox),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerSend,
            method: "AgentManagerSend",
            description: "Queue an attributed request to a scoped Epic's current lead without interrupting its active turn; the receipt does not prove acceptance or completion.",
            parameters_json: MANAGER_SEND_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerSend),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerReply,
            method: "AgentManagerReply",
            description: "Record an explicit reply to a manager request as its current feature lead; human approvals remain operator-owned.",
            parameters_json: MANAGER_REPLY_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerReply),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerNotify,
            method: "AgentManagerNotify",
            description: "Current Epic lead: queue one unsolicited notice to the current appointed manager; the daemon derives Epic, manager and scope.",
            parameters_json: MANAGER_NOTIFY_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerNotify),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerInspect,
            method: "AgentManagerInspect",
            description: "Read bounded scoped manager state and current policy fences. Workers default to actionable rows newest first: live, pending work/jobs/landings/questions, and recent (7 days) failures or unpublished sandbox results. Set include_history:true for all retained workers in creation order. Cursors bind the view; traversal completeness is not feature acceptance.",
            parameters_json: manager_v2::INSPECT.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerInspect),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerUpdate,
            method: "AgentManagerUpdate",
            description: "Record scoped work, request lifecycle, evidence, dependencies, ownership, decisions or handoff under an explicit grant; human answers remain operator-owned.",
            parameters_json: manager_v2::UPDATE.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerUpdate),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SubmitReviewReceipt,
            method: "AgentSubmitReviewReceipt",
            description: "Submit one immutable exact-source review receipt as the live assigned reviewer; caller identity, invocation, and custody are daemon-bound.",
            parameters_json: manager_v2::SUBMIT_REVIEW.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlSubmitReviewReceipt),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerControl,
            method: "AgentManagerControl",
            description: "Request a scoped lead, container, session or lead-assignment operation under an explicit grant and current fences; a queued receipt is not an effected action. Withdraw a still-queued create with withdraw_queued_create {operation_id, expected_row_version} under SessionCreate. Once claimed or started, use AgentHalt on its target_session_id.",
            parameters_json: manager_v2::CONTROL.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerControl),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerPrepareControl,
            method: "AgentManagerPrepareControl",
            description: "Prepare one supported semantic manager action against daemon-resolved live authority and return bounded readiness without queueing an effect.",
            parameters_json: manager_v2::PREPARE_CONTROL.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerPrepareControl),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerCommitPreparedControl,
            method: "AgentManagerCommitPreparedControl",
            description: "Commit an exact unexpired preparation by id and digest; mutable authority and target state are rechecked atomically before one legacy action is queued.",
            parameters_json: manager_v2::COMMIT_PREPARED_CONTROL.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerCommitPreparedControl),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerGetAction,
            method: "AgentManagerGetAction",
            description: "Read one manager action receipt within the authenticated current manager's project and logical manager scope. A queued action that a waiting deploy holds also carries held {reason: deploy_draining, deploy_id, release_by}. A queued create_session (Issue-worker launches included) that the daemon holds while the host's 1-minute load is above the operator's host_load_admission_threshold carries held {reason: host_load, load, threshold, recent_admissions}: it stays queued, never refused, and starts on its own when the load drops, held creates released oldest-first across projects. The same hold applies while the host CPU is at or above job_thermal_ceiling_c (held {reason: thermal, temperature_c, ceiling_c}) or free disk is below the governor min_free_disk_gb floor plus worker_build_reserve_gb (held {reason: disk_pressure, free_disk_gb, min_free_disk_gb, reserve_gb, needed_free_gb}, needed = floor + reserve). Build/test jobs keep the governor floor alone. A create the sandbox allocator refused for free space (the operator-configured sandbox floor) is held the same way (held {reason: sandbox_free_space, free_disk_gb, min_free_disk_gb}) and retried on its own; a refusal at the sandbox root-count cap ends blocked manager_v2_sandbox_source_root_limit.",
            parameters_json: manager_v2::GET_ACTION.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerGetAction),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerLaunchIssueWorker,
            method: "AgentManagerLaunchIssueWorker",
            description: "Manager seat (#1100): launch one worker bound to one Issue in a single idempotent call. The daemon runs the create_session action (same authority, allowlist, capacity and Epic scope as AgentManagerControl; refused before any effect), appends an Issue note naming the worker, sets the Issue InProgress and arms your on_terminal watch on the worker once it exists. Needs the SessionCreate and IssueCoordinate grants. The bound worker reads its own Issue, the review Issues launched with review_of = it and the same-project Issues it references (a `#N` or `#A-#B` selector in the Issue's body or handoffs or in the admitted request.brief field, or listed in `related`) with AgentGetIssue, all read-only (#218); it publishes to its own Issue only, and the compiled brief names that Issue as the only publication target whatever the brief says. References only in daemon-copied records grant no additional live Issue reads: the whole compiled query is not scanned. Explicitly select required same-project dependencies in request.brief (a valid #N) or related. Do not paste Issue text into the brief: pass `related` to attach complete texts; a launch of an Issue that has review Issues appends the latest review of each (complete, never cut mid-finding; one too large is named for the worker to read) to the brief (#1845); it also includes up to five recent commits from the bounded origin/rolling history whose messages name the Issue, in the receipt and worker brief (#1988); Git discovery failure returns an empty list and never blocks launch. While it is live it may also publish its handoff to that Issue with AgentUpdateIssue {handoff} (no other field, status or Issue). One worker per Issue (#2212): a launch is refused manager_issue_worker_already_live, naming the worker, while another launch is running or queued or its worker is running, or has ended its turn but is parked on a wake or daemon job it owns and has not written its final PIPELINE HANDOFF; continue_from that worker is the explicit way to replace it. A replay under the same idempotency_key returns the same worker. Poll the returned action with AgentManagerGetAction. The worker's sandbox branches from the freshly fetched origin/rolling tip by default, except review_of uses the latest implementer handoff's declared commit when available locally (an explicit sandbox_source always wins); pass sandbox_source {\"path\": \"<your sandbox_root>\"} to build on your unlanded commits, or {\"commit\": \"<40-hex>\"} for an exact commit. For either explicit source, the returned action receipt has the frozen pin in `action.sandbox_source.commit`; include that commit as the worker brief's scoped-test base so verification covers only work after launch. For the default rolling source, use `origin/rolling`. The merge-queue gate still compares the full candidate against rolling. For a review Issue pass review_of: <implementer Issue number>: the daemon copies that Issue's text and complete latest handoff into the reviewer's brief (review_of alone grants no reverse live read of that Issue; explicit selection can independently grant it; never paste a truncated copy). With no declared source commit, or one unavailable locally, the reviewer stays on rolling and its brief disallows test jobs for source tests that may be missing from that tree. Bundled pre-merge and post-land review nodes pin to their accepted source commit. When a worker passed the baton (its context filled; you got a worker_context_cap notice) and has ended its turn, pass continue_from: <that worker's session id> to relaunch from its committed HEAD with its lineage and handoff in one call. Explicit unknown sandbox_source commits are refused. review_of and continue_from are mutually exclusive; omit continue_from to launch a fresh review with the latest implementer handoff.",
            parameters_json: MANAGER_LAUNCH_ISSUE_WORKER_SCHEMA.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerLaunchIssueWorker),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerWorkView,
            method: "AgentManagerWorkView",
            description: "Read your Epic's live work, granted file ownership, pause and unanswered-request delivery state as a session the current manager created; read-only, no message bodies.",
            parameters_json: MANAGER_WORK_VIEW_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerWorkView),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerDelegateNode,
            method: "AgentManagerDelegateNode",
            description: "Create or replace one direct child node with a strictly narrower live grant; the caller must execute the current parent seat.",
            parameters_json: MANAGER_DELEGATE_NODE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerDelegateNode),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerEscalate,
            method: "AgentManagerEscalate",
            description: "Escalate one subject to the parent node or the nearest common ancestor of two Epic owners, with exact source and target fences.",
            parameters_json: MANAGER_ESCALATE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerEscalate),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerListEscalations,
            method: "AgentManagerListEscalations",
            description: "List escalations addressed to or sent by your live manager node; rulings stay on the logical node across seat succession.",
            parameters_json: MANAGER_LIST_ESCALATIONS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerListEscalations),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerResolveEscalation,
            method: "AgentManagerResolveEscalation",
            description: "Rule on an addressed escalation or forward it to your parent with exact custody and version fences; human approvals remain operator-owned.",
            parameters_json: MANAGER_RESOLVE_ESCALATION_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerResolveEscalation),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyUpsert,
            method: "AgentTopologyUpsert",
            description: "Create, revise or validate a scoped deterministic topology (current manager with Automation, or an Epic lead for its own Epic). Every session/review triple must be explicit and operator-allowed; returns the definition digest and diagnostics. Any other live session may pass validate_only:true to check a definition it designs: it writes nothing, returns the structural diagnostics in full and reports a policy problem only as \"launch not granted\", without the grant.",
            parameters_json: TOPOLOGY_UPSERT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyUpsert),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyList,
            method: "AgentTopologyList",
            description: "Page the topologies (and optionally executions) visible in your manager or Epic-lead scope.",
            parameters_json: TOPOLOGY_LIST_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyList),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyGet,
            method: "AgentTopologyGet",
            description: "Read one topology visible in your manager or Epic-lead scope by topology_id or name: the stored definition JSON, digest, revision, owner and shared flag. Use it to improve a topology (AgentTopologyUpsert with expected_revision) or fork it under a new name.",
            parameters_json: TOPOLOGY_GET_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyGet),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyExecute,
            method: "AgentTopologyExecute",
            description: "Returns advisory warnings naming live queue entries linked to inputs.issue (queued/admitted/gating); execution remains accepted. Start one daemon-run execution of a visible topology on an Epic in your scope, fenced by its definition digest; policy is rechecked and created sessions are charged. Idempotent on the key. Arms one topology_execution_settled resume wake to you by default (a replay arms none; wake:\"none\" opts out), so the settle wakes you without a separate AgentScheduleWake.",
            parameters_json: TOPOLOGY_EXECUTE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyExecute),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyGetExecution,
            method: "AgentTopologyGetExecution",
            description: "Read one execution in your scope: status, row_version, node attempts and a page of audit events.",
            parameters_json: TOPOLOGY_GET_EXECUTION_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyGetExecution),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyInterrupt,
            method: "AgentTopologyInterrupt",
            description: "Request interruption of one execution in your scope under a row-version CAS; running node sessions are interrupted and the execution settles.",
            parameters_json: TOPOLOGY_INTERRUPT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyInterrupt),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyResolveAttempt,
            method: "AgentTopologyResolveAttempt",
            description: "Resolve an attempt blocked on preserved work: inspect, accept or retry (manager or Epic lead in scope); discard needs the manager with Automation and the exact preserved commit.",
            parameters_json: TOPOLOGY_RESOLVE_ATTEMPT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlTopologyResolveAttempt),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::EnqueueLandingSource,
            method: "AgentEnqueueLandingSource",
            description: "Enqueue one accepted source commit on the daemon-owned rolling merge queue (current appointed manager or current Epic lead). Pass derive_filters:true so the queue derives the test filters itself (no hand-copied, pruned or split check-touched-shards output; explicit test_filters are capped at 32 filters of 256 bytes). The queue gates and publishes it; the outcome wakes you once. One call is enough: when the filter derivation outlasts the call (a Rust diff takes 10-45 s) the entry is recorded queued with filters_state deriving, is admitted only after its filters land; a derivation that fails settles it refused (queue_filter_derivation_failed) with that same single wake, while one that exceeds its budget (a very wide diff) is admitted with filters_state lander_selects and no derived filters, the lander selecting the tests itself; end your turn, never sleep, retry or poll. A fix for known-red tests passes fixes_issue (the red Issue) and/or fixes_tests: the gate then requires those tests green and refuses claimed_fix_still_red otherwise. A manager may pass expedite {reason} to admit the entry into the next batch ahead of ordinary queued entries (at most 2 live per project; a fixes_issue naming an open qa Issue expedites automatically). Optional issue records the source project’s Issue display number on the entry and in AgentListRollingQueue; AgentTopologyExecute on it warns about a live landing without refusing. Optional supersedes names a queued entry you may cancel (including a predecessor manager seat) to replace with a descendant commit at the same queue position; admitted/gating entries return queue_entry_in_flight: enqueue normally without supersedes. Refused with queue_disabled while the operator has the queue off.",
            parameters_json: ENQUEUE_LANDING_SOURCE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlEnqueueLandingSource),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListRollingQueue,
            method: "AgentListRollingQueue",
            description: "Each entry includes its optional source-project Issue display number. List rolling merge queue entries in your scope, newest first. Default: queued/admitted/gating plus refused/failed entries finished in the last hour; outcomes retain landed_sha, refusal code and a one-line summary (160 bytes). fields:\"full\" or an explicit state restores all matching history and the existing outcome detail (#1964; the current appointed manager of the project, a portfolio seat inside its grant, or the session an entry's outcome would wake). Each row: sequence, state, source commit, enqueuer, wake target, batch id and a bounded outcome summary, plus qa_green: the latest rolling SHA a full-library QA run proved green (sha, passed_at, job_id; null before any pass), and held_for_deploy {deploy_id, release_by} while a staged hub deploy holds queue admission (the gating batch finishes, nothing new is admitted until the deploy settles or its deadline passes; #3885). Filter by state or project; page with before_sequence. Read-only; the operator-wide view stays operator-only.",
            parameters_json: LIST_ROLLING_QUEUE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListRollingQueue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CancelRollingQueueEntry,
            method: "AgentCancelRollingQueueEntry",
            description: "Withdraw one still-queued rolling merge queue entry (#1964): a compare-and-swap queued -> superseded with outcome queue_entry_cancelled, by the project's current appointed manager (the seat is followed across a rotation), a portfolio seat in Execute mode inside its grant, or the session the entry's outcome would wake. An admitted or gating entry belongs to the lander and is refused queue_entry_in_flight; a settled one is refused queue_entry_settled. A cancel replays as success. The entry's owner gets one wake naming the cancel when it is not you. Re-enqueue the commit with a new idempotency_key.",
            parameters_json: CANCEL_ROLLING_QUEUE_ENTRY_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlCancelRollingQueueEntry),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CheckLandingCandidate,
            method: "AgentCheckLandingCandidate",
            description: "Check, before AgentEnqueueLandingSource, whether a worker's scoped_test job verifies source_commit (#2310; the worker's appointed manager, a portfolio seat over its project, or the Epic lead; read-only). Name the job (full id or a unique prefix of at least 8 hex) or the worker's source_session_id. Returns ok, a verdict refusal (candidate_no_verification_job, candidate_job_not_terminal, candidate_admission_timeout, candidate_receipt_missing, candidate_tool_untrusted, candidate_job_not_green, candidate_new_failures, candidate_head_mismatch, candidate_conflicts_with_rolling), the job id, kind, state, exit code and own refusal, the receipt head and whether it equals source_commit, new_failures, baseline_reds, tests_completed, the plan (precise or targeted_subset, gate_covers_rest), and, for a head mismatch, whether the delta between the receipt head and source_commit touches documentation only. The receipt's merges_cleanly_onto_rolling and conflict_paths (#348) and a fresh, fetch-free trial merge of source_commit onto the local origin/rolling (merges_cleanly_onto_rolling_now, conflict_paths_now) are reported; a conflict is the last verdict refusal, candidate_conflicts_with_rolling, and its detail names the paths. Daemon-written scoped_tool exposes source, path, rev and tool file blobs. Candidate/sandbox tools and legacy jobs without provenance require a rolling-tool rerun (params.scoped_test.rolling_tool:true); another acceptable rolling-tool job from the same owner at source_commit supplies verification and tool_under_test retains the original job and provenance. Baseline reds alone do not block: the landing gate is no new failures relative to rolling. A caller outside those seats is refused candidate_not_authorized; an unknown or out-of-scope job or session is refused candidate_not_found.",
            parameters_json: CHECK_LANDING_CANDIDATE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlCheckLandingCandidate),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReadSessionEvents,
            method: "AgentReadSessionEvents",
            description: "Read a bounded page of a session's conversation events (tail or after_sequence), with final_message and terminal_reason, for your own child, a child of an Epic you lead, a session in your manager scope, or (Issue-bound worker, while the binding is live) a same-project non-manager session that created your bound Issue or whose full lowercase UUID is cited in its body, launch brief or newest 64 handoffs authored outside your continuation lineage. Manager seats, including former seats and their continuations, are excluded. Set final_message_full to return up to 32768 characters of the final message; set full_fields to raise the per-event content, tool_input and metadata clip from 2000 to 262144 characters.",
            parameters_json: READ_SESSION_EVENTS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlReadSessionEvents),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetProviderStatus,
            method: "AgentGetProviderStatus",
            description: "Read bounded, secret-free provider health for the current appointed manager or Epic lead: configured, reachable, remaining credit where the provider API exposes it (OpenRouter), last 402/429 time, recent launch failure rate, and whether a launch would be refused. The openrouter entry also carries spend_cap (#3154: the operator cap, its baseline, spend since arming, the remainder, the warn percent and the state off/ok/warn/reached/probe_stale; check it before every paid run) and, when you name models with provider openrouter, model_prices from the daemon's own model discovery, and, with run_budget_usd (and run_sessions), run_budget: whether a benchmark run may launch its next task. The daemon calls provider APIs with its own credentials (cached 60 s) and never returns a key. RPC-only.",
            parameters_json: GET_PROVIDER_STATUS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGetProviderStatus),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SubmitJob,
            method: "AgentSubmitJob",
            description: "Run a long test (including a declared just/make recipe), build, landing or cloud-gate operation as a daemon-owned durable job in your sandbox. Linux uses systemd; macOS uses launchd for package test/build and recipe jobs (shard, candidate-receipt, landing and cloud workflows, and other operating systems, are refused job_platform_unsupported). Package test params accept exact:true for whole test-name matching (libtest --exact; default false keeps substring filters), and test_target for one integration test target (requires exact:true and nonempty filters; mutually exclusive with lib_only). It survives your turn ending, your session and a daemon restart; one resume wake carries the typed result (wake none suppresses that per-job wake: submit a batch with wake none, then arm one AgentScheduleWake mode when with when.jobs_terminal for a single wake). The timeout (timeout_minutes, default job_test_timeout_mins) starts at the wrapper's job-start. Test jobs, including scoped_test, exclude build-slot and sandbox/artifact-lock waits from execution. Scoped receipts report the separate queue and admission waits. A scoped_test diff selecting no Rust package takes no governor slot. Local test/build peers from the same owner and sandbox queue in submission order before unit launch, without spending build admission or execution time. Unit start has a separate ten-minute cap (job_start_timed_out); build admission lasts the job's own timeout (at least ten minutes) for test jobs including scoped_test, or one hour for a build job (job_admission_timed_out), so a job queued behind others on the governor is not failed after a fixed ten minutes. Scoped jobs also have a finite pre-launch admission allowance equal to their build admission allowance, even during a deploy hold; deploy reruns start a new queue allowance. Receipts retain the last phase. While a hub deploy is draining, or a staged interrupt_workers deploy is pending (a running test job keeps running across its restart), a new test or build job is accepted but held (held=held_for_deploy, receipt release_by): no unit starts and no execution clock runs; the scoped pre-launch admission cap still applies. It starts in submission order once the deploy settles or is abandoned, and its one wake fires at its real terminal state. The receipt carries the id as job_id; if a reply is cut off, list yours with AgentListJobs. Dispatch it and end your turn.",
            parameters_json: &SUBMIT_JOB_DETAILED_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlSubmitJob),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetJob,
            method: "AgentGetJob",
            description: "Read a daemon-owned job in your scope: state, exit code, log path and typed result. Scope includes your jobs, manager-scope and published manager-seat predecessor jobs, and Issue-worker predecessor test/build jobs proven by the same Issue, project and frozen source/custody chain (#584). A published Issue-worker continuation may also watch those proven predecessor jobs; ownership, cancellation and receipt acceptance rules stay unchanged. A review_of launch may Get scoped_test jobs from the reviewed Issue's newest four eligible implementer sessions: each owner's newest, or an older one whose full UUID appears in that Issue's body or newest 64 handoffs (#604). Recipe jobs and copied IDs alone do not qualify. This review exception grants no List, watch, cancel or ownership authority. job_not_found means absent or inaccessible; it does not reveal which. Reading a receipt does not prove it verifies the reviewed SHA. A finished scoped_test job stores a receipt of about 130k characters; pass fields:\"compact\" for the bounded view (verdict, head, merge state, failures, per-package status, warnings verdict) and fields:\"full\" (the default) for the whole job.",
            parameters_json: GET_JOB_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGetJob),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListJobs,
            method: "AgentListJobs",
            description: "List the jobs you submitted, newest first (limit 1-100, default 20): recover a job id a truncated AgentSubmitJob reply lost. Rows are compact by default (id, name, kind, state, exit_code, refusal, receipt_head, held, times, log_path, wake; no result payload): read one job with AgentGetJob, or pass fields:\"full\" for every job with its whole result. A manager seat also lists its rotation predecessors' jobs (#2470); a published Issue-worker continuation lists its scoped predecessor test/build jobs (#584). Unrelated jobs stay unlisted.",
            parameters_json: LIST_JOBS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListJobs),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CancelJob,
            method: "AgentCancelJob",
            description: "Stop one of your own running daemon-owned jobs (a mistaken or over-broad run) through its platform service label: the service is stopped and the job settles failed with refusal job_cancelled. Cancelling a job that already settled changes nothing. The current appointed manager (or a portfolio seat inside its grant) may also cancel an ordinary test or build job owned by a session in its live scope (#2213); the result names it as cancelled_by_manager and the job detail names it and the owner. A landing, cloud gate or sweep, release build or automatic full-library QA run is never cancelled by a manager (#2232). Every other job, and every refusal that would show a caller a job it has no authority over, is job_not_found (#2233).",
            parameters_json: CANCEL_JOB_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlCancelJob),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SendSatelliteMessage,
            method: "AgentSendSatelliteMessage",
            description: "Queue one message for an idle session on a paired satellite host (current appointed manager only; the operator must enable dispatch and declare the target in scope). Delivery waits until the remote session is idle and never interrupts; `queued` is acceptance, not delivery. `issue_refs` expands up to 8 of your own project's Issues, whole and read-only, into the message (the satellite cannot read hub Issues itself; this is the one way their body and acceptance reach it). Every peer or target refusal is the same target_not_authorized; an unreadable Issue ref is one satellite_issue_ref_unavailable, raised only after the target was authorized.",
            parameters_json: SEND_SATELLITE_MESSAGE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlSendSatelliteMessage),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReportToHub,
            method: "AgentReportToHub",
            description: "Queue one short typed report (enqueue, deploy_ready or result) for the hub manager (#1103). Satellite-side only: the current appointed manager that is also the operator-declared seat (or its rotation tip), and the operator must allowlist a hub. The hub pulls reports over the existing link and shows them in its AgentManagerInbox as an untrusted, informational satellite report that carries no authority; `queued` is acceptance, not delivery (at most once; a restart loses unsent reports). Every authorization failure is the same satellite_report_not_authorized. RPC-only.",
            parameters_json: REPORT_TO_HUB_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetDaemonInfo,
            method: "AgentGetDaemonInfo",
            description: "Read the running hub daemon's identity and health for the current appointed manager or Epic lead: embedded build SHA, sha256 of the running binary, start time, schema version, disk free for the data dir and sandbox base, 1/5/15 load average, and supervisor mode. `host_load` reports the host-load admission (threshold, supported, load, recent_admissions, holding and the held manager creates and topology nodes, oldest first); its `build_disk` {min_free_gb, free_gb, open} is the governor disk floor every build/test job waits on in admission (separate from a launch floor), so read it before promising verification. `target_pool` is the warm target pool status: the newest pool and, when no pool was built, the typed reason (`last_skip` pool_skipped_disk_headroom with free/floor/expected GiB; a failed build with reason pool_aborted_disk_floor) and `last_reclaim` (idle pools evicted for a worker waiting on the disk floor). The appointed manager also gets `satellites`: each enabled paired satellite read over the link now (build_sha, binary_sha256, started_at, schema_version, supervisor_mode, last_deploy, launches (`none` when a manager seat has no launch choices: the hub must push a list), or reachable:false). Secret-free: never returns an environment value. RPC-only.",
            parameters_json: GET_DAEMON_INFO_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGetDaemonInfo),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::PushSatelliteManagerPolicy,
            method: "AgentPushSatelliteManagerPolicy",
            description: "Push explicit launch choices and session caps to a paired, dispatch-enabled satellite manager in declared scope, using the deploy link and Deploy authority (Execute, unpaused). Every authorization failure is target_not_authorized. Choices and caps must fit the hub's effective project grant; widening is refused. Expected satellite scope/policy versions prevent overwriting a later operator save. Exact retries return a durable provenance receipt. Other policy fields remain operator-owned. RPC-only.",
            parameters_json: PUSH_SATELLITE_POLICY_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReseatSatelliteManager,
            method: "AgentReseatSatelliteManager",
            description: "Move a paired satellite's failed manager seat to another provider in one step (#2750), using the deploy link and Deploy authority (Execute, unpaused). A seat whose provider is down (a lost login, a quota) cannot succeed itself and a session's provider is fixed at creation. The satellite resolves the declared address to its live seat, launches a successor on `provider` (optionally `model`/`effort`) with continued_from set to that seat, re-appoints it with the project's saved scope and policy (wakes and open requests follow) and records `reason`. The seat must be Failed, or Completed/Interrupted with provider_unavailable true; a running seat is refused. Your declared scope needs no edit: the address you already use resolves to the successor through lineage. Every authorization failure is target_not_authorized. A retry after success finds the healthy successor and is refused. RPC-only.",
            parameters_json: RESEAT_SATELLITE_MANAGER_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::RequestDeploy,
            method: "AgentRequestDeploy",
            description: "Ask the daemon to deploy already-built binaries (appointed manager with Deploy, Execute, not paused). Stages and verifies binaries, waits for a quiet point bounded by max_wait_secs, swaps them and restarts under the supervisor; one outcome wake. With peer_id, omitted interrupt_workers defaults to true: after the satellite's deploy_drain_hold_secs (default 600), mid-turn workers stop blocking and restart interrupts and resumes them through the journal. Explicit false waits for a lull. Local hub deploys keep default false; explicit true opts in. Keep max_wait_secs above the hold. Ordinary test/scoped_test jobs are never stopped or rerun: their units keep running across the restart and the new daemon re-adopts them (one wake each, when the job ends); landing, build and protected jobs still block. Receipts name expected_interrupted_workers and expected_surviving_jobs while staged, actual interrupted_workers and surviving_jobs after effects; the outcome wake names actual sessions and the jobs that kept running. A peer without deploy_interrupt is refused satellite_deploy_interrupt_unsupported; explicit false is compatible. Cancel your waiting deploy with its sha, idempotency_key and cancel:true, omitting interrupt_workers; cancellation has no interrupt default. Peer cancellation applies satellite ownership checks; deploy_cancel_too_late once restart began or settled. The hub stores no peer deploy state: confirm via AgentGetDaemonInfo satellites build_sha/last_deploy and the outcome wake. RPC-only.",
            parameters_json: REQUEST_DEPLOY_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlRequestDeploy),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SwitchHostPreset,
            method: "AgentSwitchHostPreset",
            description: "Switch this host to an operator-saved resource preset, or undo its last switch. Current appointed project/area/portfolio manager seat only, Execute and unpaused. Accepts only action:apply with name, or action:undo; never raw values or preset edits. Operator tuning bounds and explicit manager per-key bounds bind apply and undo. Re-applying matching values is idempotent. Returns actual settings, apply classes, probe and saved undo state; complete:false retains undo on a partial failure. RPC-only. Refusals: host_tuning_manager_required, host_tuning_preset_unknown, host_tuning_manager_bound, host_tuning_pending.",
            parameters_json: r#"{"type":"object","properties":{"action":{"type":"string","enum":["apply","undo"]},"name":{"type":"string","minLength":1,"maxLength":64}},"required":["action"],"additionalProperties":false}"#,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::QueryFailureSignatures,
            method: "AgentQueryFailureSignatures",
            description: "Before debugging a red, ask whether it is already known: read the known-failure signature records (#1016) that open Issues of your project carry, by exact test_id and/or failure digest (at least one). Each record names its owner Issue (record.issue, issue_id), class (regression, flake, env, seed) and matcher; a record whose owner Issue is Closed, Cancelled or archived is never returned, so expiry is live. Read-only, project-scoped, open to every session; no Issue body is returned. An unknown query returns records [] (a clean, not-known answer); malformed_issues lists open Issues whose signature blocks did not parse.",
            parameters_json: QUERY_FAILURE_SIGNATURES_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlQueryFailureSignatures),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::WorkerRoster,
            method: "AgentWorkerRoster",
            description: "Read worker states in your scope; default summary omits finished history older than one hour. A completed reviewer with a submitted receipt or a completed node of a settled topology execution is final_handoff once no wake is owed. Each row (#2311): session id, bound Issue number, provider/model/effort, derived state (running, parked_on_job, parked_on_wake, stranded, final_handoff, failed), its newest daemon job (id, kind, state, exit_code, refusal, receipt_head), its open job count, its armed wakes (mode, when-predicate, next_fire_at), last activity time and whether the latest turn carries a final PIPELINE HANDOFF. A `Completed` session is parked on its job or wake, stranded (nothing will resume it) or finished: the roster tells them apart. Scope: the workers you launched yourself; a project's appointed manager gets its whole project by default, and project_id names a project you manage (a portfolio seat: one inside its grant). Default summary: live workers (strands idle over seven days stay hidden in stale_stranded_hidden) plus final_handoff/failed active within the last hour; older finished history is counted in hidden_finished. Nonempty `states` or fields:\"full\" restores matching history. `count` and `counts` cover returned workers only; hidden counts cover examined candidates before the page/512-candidate bound, and `truncated` signals more matching or unexamined workers or queued launches. Read-only; no receipt check (read the job's receipt_head and compare it yourself).",
            parameters_json: WORKER_ROSTER_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlWorkerRoster),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GlobalOverview,
            method: "AgentGlobalOverview",
            description: "Global manager only (#872): one bounded read of every granted project: its current project manager (status, provider, model, context fill %, cost, updated_at), that manager's policy (mode, revoked, paused, capabilities), Issue counts (Open, InProgress, Open operator-request), Running and WaitingApproval session counts, and pending questions and approvals. Read-only.",
            parameters_json: GLOBAL_OVERVIEW_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGlobalOverview),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GlobalSend,
            method: "AgentGlobalSend",
            description: "Global manager only (#872): queue durable mail to a granted project's current project manager. It is delivered at that manager's next idle boundary and wakes it when idle. A replay under the same idempotency_key returns the same message_id.",
            parameters_json: GLOBAL_SEND_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGlobalSend),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GlobalAppointManager,
            method: "AgentGlobalAppointManager",
            description: "Global manager only (#872): in one idempotent call, launch a Standard root session in a granted project (launch must be in your grant's allowed_launches; checked before any effect), appoint it with whole-project scope (displacing the current project manager exactly as the operator's appoint does) and save your grant's project policy under the new scope version. Returns {session_id, scope_version, policy_version}; a replay returns the same session. RPC-only.",
            parameters_json: GLOBAL_APPOINT_MANAGER_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReportToGlobal,
            method: "AgentReportToGlobal",
            description: "Project manager of a project in the active global grant (#872): queue durable mail to the global manager (landed work, a gate that blocks you, your handoff path). It wakes the global manager when idle. A replay under the same idempotency_key returns the same message_id.",
            parameters_json: REPORT_TO_GLOBAL_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlReportToGlobal),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReportUp,
            method: "AgentReportUp",
            description: "Any manager seat (area, project or portfolio of any tier; #1238): queue durable mail to the manager one level above you (parent_of your node: parent area, project manager, the deepest portfolio node covering your project, or your portfolio parent). At the top of the chain it becomes an operator notice. It wakes the recipient when idle. Reports carry no authority. A replay under the same idempotency_key returns the same message_id. AgentReportToGlobal is its alias for one release.",
            parameters_json: REPORT_UP_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlReportUp),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SendDown,
            method: "AgentSendDown",
            description: "Any manager seat (#1238): queue durable mail to a descendant manager node's live seat inside your coverage: target {kind: portfolio|area, node_id} or {kind: project, project_id}. Prefer your direct children. Refused for your own node, an ancestor (manager_target_not_descendant) or anything outside your coverage (manager_project_not_in_scope). It wakes the recipient when idle. A replay under the same idempotency_key returns the same message_id. AgentGlobalSend is its alias for one release.",
            parameters_json: SEND_DOWN_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlSendDown),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerAppointChild,
            method: "AgentManagerAppointChild",
            description: "Portfolio manager seat at any tier (#1239): in one idempotent call, launch a Standard root session (launch must be in your grant's allowed_launches) and seat it as a child: the project manager of a project in your coverage (displacing the current one as the operator's appoint does, saved with your child_policy), a new child manager node over a strict subset of your coverage (it narrows your grant in every dimension; sibling coverage stays disjoint; at most max_direct_reports children and project managers under you), or the replacement seat of a child node you granted (its grant, ledger and workers stay; the old seat loses authority at once), or your own seat: node_id = your own node hands the seat to the session this call launches (#2021), whoever granted the node, fenced on expected_grant_version (the grant version advances; the grantor is preserved); the node, grant limits, authority epoch, ledger, escalations, workers, terminal watches, operator to-do scope and pending wakes carry over, the operator may still re-seat or revoke it (an operator re-seat in flight wins), and the handoff reference is recorded and shown in AgentManagerOverview. Put the handoff path in handoff and in the successor's query, then end your turn. Every refusal happens before any session is created. Only the operator creates roots, adopts or re-parents. Returns {appointment_id, session_id, target_ref, scope_version, policy_version, deduplicated}; a replay returns the same session. AgentGlobalAppointManager is its project-target alias. RPC-only.",
            parameters_json: MANAGER_APPOINT_CHILD_SCHEMA.as_str(),
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerRevokeChild,
            method: "AgentManagerRevokeChild",
            description: "Portfolio manager seat at any tier (#1239): revoke a child manager node your node granted, CAS-fenced on its grant version. Nodes whose authority came from it (grantor node:<child>) are revoked with their subtree; operator-granted descendants move up one level and keep their seats, ledgers and workers. An operator-granted child is refused (manager_child_operator_granted): only the operator revokes it. Nothing is deleted; a replay returns the revoked child. RPC-only.",
            parameters_json: MANAGER_REVOKE_CHILD_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerOverview,
            method: "AgentManagerOverview",
            description: "Any manager seat (area, project or portfolio of any tier; #1240): one bounded read of your own manager node: its grant and seat, parent_of it, each child (child portfolio nodes, the project managers you manage directly, child areas) as a digest with its seat status, model, context fill %, grant version, coverage, summed Issue and session counts and pending escalations, the projects you manage directly (project manager seat and policy, Issue counts, Running and WaitingApproval counts, pending questions and approvals), the escalations waiting on you and a fleet rollup of your coverage. `fleet` picks its detail: `summary` (default: active agents, 1h and 24h totals and one row per model with invocations, errors and cost), `none` (fleet null) or `full` (usage over 5m/1h/24h by project, provider and model). `detail` defaults to `compact` (#3531: each project manager's seat appears once, in `projects[].overview.manager`; standing directives as id, scope, title and updated_at; `coordinator_budget` as a key into `coordinator_budgets`; predecessors as `predecessor_count` plus `newest_predecessor`; launches only as `grant.allowed_launches`); `directives:true` adds the full directive text and `detail: full` returns everything. A child node's own projects and inbox are never included: send down to that child to ask. Refused for a caller that holds no manager node seat (manager_tier_not_node_seat). Read-only. AgentGlobalOverview is its v0-shaped alias.",
            parameters_json: MANAGER_OVERVIEW_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerOverview),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CreateProject,
            method: "AgentCreateProject",
            description: "Project manager (Execute mode, not paused) or portfolio seat (#1626): register a new RSI project for a directory so work can start there. Give a unique name and an absolute path to an existing directory inside a configured workspace root (with none configured: a strict descendant of the daemon user's home directory, never the home directory itself, ~/.rsi or /); description and color are optional. The project appears in the operator's project list; it has no manager yet and is not added to your coverage (the operator or your parent grants that). A repeat with the same name and directory returns the existing project with deduplicated:true only when it is in your coverage; otherwise project_name_taken. A path equal to, containing or inside the harness root is project_harness_protected. Refused: project_not_authorized, project_name_taken, project_path_taken, project_path_invalid, project_harness_protected. There is no agent verb that deletes or archives a project.",
            parameters_json: CREATE_PROJECT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlCreateProject),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::UpdateProject,
            method: "AgentUpdateProject",
            description: "Project manager (Execute mode, not paused) or portfolio seat (#1626): rename a project or change its path, description or color, limited to projects in your coverage (your own project for a project manager; your grant's projects for a portfolio seat). Send only the fields to change. A path change is refused while the project has a live session (project_has_live_sessions), and any path change on the harness project is project_harness_protected (operator-only). A seat that is both a project manager and a portfolio seat acts on the union of what its permitting seats cover. A project outside your coverage, or one that does not exist, is project_not_in_scope. Nothing is deleted.",
            parameters_json: UPDATE_PROJECT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlUpdateProject),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::OperatorTodoUpsert,
            method: "AgentOperatorTodoUpsert",
            description: "Any manager seat (area, project or portfolio of any tier; #1747): put a thing only the operator can do on the operator's to-do list instead of repeating it in chat. Give a stable todo_key, a one-line title, the exact command or keys in how, an optional link (issue:#N, session:<uuid> or a URL) and a priority (1 most urgent, default 3). The daemon derives the item's scope from your seat (your project, area or portfolio node); you cannot name another. The same todo_key in your scope updates the open item; a closed item stays closed (operator_todo_closed): use a new key. A replay under the same idempotency_key returns the same item. At most 64 open items per scope (operator_todo_limit). Refused: operator_todo_not_authorized, operator_todo_invalid_request.",
            parameters_json: OPERATOR_TODO_UPSERT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlOperatorTodoUpsert),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::OperatorTodoClose,
            method: "AgentOperatorTodoClose",
            description: "Any manager seat (#1747): close an operator to-do item when you observe it was done (status done, observed true, evidence = what you saw) or no longer applies (status dismissed, evidence = why). Evidence is always recorded on the item; you never mark an item done on the operator's behalf. Name the item by todo_id or by the todo_key of an item in your own scope. A project or area seat closes only items of its own scope (operator_todo_not_in_scope); a portfolio seat may close any item by todo_id. A replay under the same idempotency_key returns the same closed item, and so does a close of an item that is already closed (the operator or another seat closed it first; nothing is written and the reply carries the recorded status and evidence). Refused: operator_todo_evidence_required, operator_todo_not_found, operator_todo_not_authorized.",
            parameters_json: OPERATOR_TODO_CLOSE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlOperatorTodoClose),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::StandingDirective,
            method: "AgentStandingDirective",
            description: "Any manager seat (#2649): relay an operator's standing instruction to a manager scope strictly below your own so it survives every seat rotation without a hand-copied handoff, or, with kind manager (#3736), your own standing ruling for it (a capacity throttle, a review-route rule), stored with your seat as author and shown to every seat as manager-authored, never as operator words; or clear one a manager relayed or ruled. The daemon holds the directive on the scope (project:<uuid>, area:<uuid> or portfolio:<uuid>), not on a session, and puts it verbatim in the authority catalog of every seat of that scope, in a context-cap successor's launch prompt, and in AgentManagerOverview / AgentGlobalOverview. action set needs scope, text (the operator's words, at most 2048 bytes), provenance (where and when the operator said it: a relay without it is refused; with kind manager it is an optional note), idempotency_key, and may carry expires_in_secs (for a while-away directive; absent means until cleared); at most 8 active per scope. action clear needs only directive_id. You cannot write your own scope, and a directive the operator wrote only the operator clears, and a manager ruling only the manager scope that wrote it clears. The author is derived from your seat, never read from the request. Refused: standing_directive_not_authorized, standing_directive_invalid_request, standing_directive_not_found, standing_directive_limit, standing_directive_idempotency_conflict, manager_tier_target_unknown.",
            parameters_json: STANDING_DIRECTIVE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlStandingDirective),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListSourceWorktreeCohorts,
            method: "AgentListSourceWorktreeCohorts",
            description: "List source-worktree cohorts covered by your operator-granted storage_control capability. Only a current Execute-mode, unpaused project manager or portfolio seat may call this. Scope is checked per root: every foreign owner, participant and receipt owner, regardless of status, is retained with a typed audit disposition. Only covered terminal owners without live consumers may settle through these agent controls. Runs locally on the hub or satellite under its own seat policy.",
            parameters_json: LIST_SOURCE_WORKTREE_COHORTS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListSourceWorktreeCohorts),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::AuditSourceWorktreeCohort,
            method: "AgentAuditSourceWorktreeCohort",
            description: "Audit a covered repository cohort without writes, using the operator settlement proof. Requires storage_control on a current Execute-mode, unpaused PM or portfolio seat. fields summary returns counts_by_disposition, applyable, plan_digest, authorization_phrase, only eligible items and last_run_retained_root_count (within your coverage); fields full keeps every item and last_run_retained_roots. Omitted fields defaults to summary when nothing is applyable, full otherwise. Summary preserves the exact apply inputs. Only terminal, clean, integrated-ancestor sandbox worktrees with no live custody or path dependencies can be eligible. Dirty and non-ancestor work is retained.",
            parameters_json: AUDIT_SOURCE_WORKTREE_COHORT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlAuditSourceWorktreeCohort),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ApplySourceWorktreeCohort,
            method: "AgentApplySourceWorktreeCohort",
            description: "Settle eligible merged source worktrees yourself when disk is low; never ask the operator to perform settlement. Requires operator-granted storage_control on a current Execute-mode, unpaused PM or portfolio seat associated with the repository; ownership is checked per root. Send the audit plan_digest, exact authorization_phrase as authorization and a stable idempotency_key. Calls the same audited apply as the operator: records durable intent, rechecks authority and Git/custody proof before removing only clean terminal integrated-ancestor sandbox roots and their exact source branches. Returns at once (within seconds, however many roots) with the durable run receipt while the run keeps applying in the daemon (finished_at is unset while it runs; a whole cohort can take many minutes, so never wait on this call); exact retries return the run's current receipt and never start a second driver. To wait for the end without polling, arm AgentScheduleWake mode when with when.settlement_run_settled set to the receipt's run_id; the wake reports the run state and per-phase counts, and a retry of this call then returns the finished receipt. Dirty, non-ancestor and dependent work is retained. No database rows are deleted.",
            parameters_json: APPLY_SOURCE_WORKTREE_COHORT_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlApplySourceWorktreeCohort),
        },
    ]
});

/// Iterate the only machine-readable method catalog exposed by `rsi-common`.
#[must_use]
pub fn agent_control_catalog_v1() -> &'static [AgentControlDescriptorV1] {
    &*AGENT_CONTROL_CATALOG_V1
}

/// #3565: the native tools a manager seat uses on nearly every wake. Claude
/// Code defers MCP tools behind `ToolSearch`, so each seat re-loaded the same
/// schemas (17-20k tokens per seat). The `rsi-agent-mcp` gateway marks exactly
/// these tools `_meta["anthropic/alwaysLoad"] = true` for a manager seat, so
/// they are in the prompt from the first turn; every other tool stays
/// deferred, and a worker preloads none.
pub const MANAGER_PRELOADED_NATIVE_TOOLS: &[&str] = &[
    "rsi_control_authority_catalog",
    "rsi_control_manager_inbox",
    "rsi_control_manager_overview",
    "rsi_control_list_wakes",
    "schedule_wake",
    "rsi_control_list_issues",
    "rsi_control_get_issue",
    "rsi_control_update_issue",
    "rsi_control_create_issue",
    "rsi_control_list_rolling_queue",
    "rsi_control_check_landing_candidate",
    "rsi_control_topology_execute",
    "rsi_control_topology_get_execution",
    "rsi_control_manager_send",
    "rsi_control_report_up",
    "rsi_control_read_session_events",
    "rsi_control_worker_roster",
    "rsi_control_status",
    "rsi_control_enqueue_landing_source",
    "rsi_control_send_satellite_message",
    "rsi_control_request_deploy",
    "rsi_control_get_daemon_info",
    "rsi_control_operator_todo_upsert",
];

/// Catalog roles (`AgentAuthorityCatalogV1.roles`) of a manager seat: the
/// project manager and the portfolio/global manager.
pub const MANAGER_PRELOAD_ROLES: &[&str] = &["manager", "global_manager"];

/// The MCP tool `_meta` key Claude Code reads to load a tool without
/// `ToolSearch` (`true`) or to defer it.
pub const MCP_ALWAYS_LOAD_META_KEY: &str = "anthropic/alwaysLoad";

/// Whether `roles` (from the live authority catalog) name a manager seat.
#[must_use]
pub fn roles_preload_manager_tools<S: AsRef<str>>(roles: &[S]) -> bool {
    roles
        .iter()
        .any(|role| MANAGER_PRELOAD_ROLES.contains(&role.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3565: every preloaded manager tool is a real native tool, once.
    #[test]
    fn manager_preloaded_tools_are_native_tools_without_repeats() {
        let native = crate::rpc_verb_registry::native_tool_names();
        let mut seen = std::collections::BTreeSet::new();
        for name in MANAGER_PRELOADED_NATIVE_TOOLS {
            assert!(native.contains(name), "{name} is not a native tool");
            assert!(seen.insert(*name), "{name} listed twice");
        }
        assert!(roles_preload_manager_tools(&["worker", "manager"]));
        assert!(roles_preload_manager_tools(&["worker", "global_manager"]));
        assert!(!roles_preload_manager_tools(&["worker", "epic_lead"]));
        assert!(!roles_preload_manager_tools::<&str>(&[]));
    }

    #[test]
    fn delegation_reports_the_grant_field_for_invalid_direct_report_cap() {
        let verb = AgentControlVerbV1::ManagerDelegateNode;
        let mut params = verb.example();
        for cap in [0, 65] {
            params["grant"]["max_direct_reports"] = serde_json::json!(cap);
            let error = verb.validate_params(&params).unwrap_err();
            assert_eq!(
                error.path.as_deref(),
                Some("params.grant.max_direct_reports")
            );
            assert!(error.constraint.is_some());
        }
    }

    #[test]
    fn allowed_launches_schema_caps_match_daemon_limit() {
        let cap = crate::global_manager::GLOBAL_MANAGER_MAX_LAUNCHES as u64;
        let push: Value = serde_json::from_str(PUSH_SATELLITE_POLICY_SCHEMA).unwrap();
        assert_eq!(
            push["properties"]["policy"]["properties"]["allowed_launches"]["maxItems"],
            cap
        );
        let appoint: Value = serde_json::from_str(&MANAGER_APPOINT_CHILD_SCHEMA).unwrap();
        assert_eq!(
            appoint["properties"]["target"]["oneOf"][1]["properties"]["allowed_launches"]["maxItems"],
            cap
        );
    }

    use crate::agent_coordination::{
        AgentArchiveChildRequestV1, AgentContinueChildRequestV1, AgentGetProgressParamsV1,
        AgentReserveSuccessorRequestV1, AgentSendMessageRequestV1, AgentSpawnChildRequestV1,
    };
    use crate::harness_manager::{
        AgentManagerInboxRequestV1, AgentManagerNotifyRequestV1, AgentManagerProgressRequestV1,
        AgentManagerReplyRequestV1, AgentManagerSendRequestV1, AgentManagerWorkViewRequestV1,
        HARNESS_MANAGER_MAX_INBOX_PAGE, HARNESS_MANAGER_MAX_MESSAGE_BYTES,
        validate_manager_message,
    };
    use crate::rpc::ResolveTopologyAttemptParams;
    use crate::rpc::{
        AgentArchiveIssueRequestV1, AgentCreateIssueParams, AgentGetIssueRequestV1,
        AgentListIssuesRequestV1, AgentRestoreIssueRequestV1, AgentUpdateIssueRequestV1,
        AgentUpdateIssueStatusRequestV1,
    };
    use crate::topology_agent::{
        AgentTopologyExecuteRequestV1, AgentTopologyGetExecutionRequestV1,
        AgentTopologyGetRequestV1, AgentTopologyInterruptRequestV1, AgentTopologyListRequestV1,
        AgentTopologyUpsertRequestV1,
    };
    use crate::types::IssueEventPageRequestV1;
    use std::collections::BTreeSet;

    fn fixture(verb: AgentControlVerbV1) -> Value {
        verb.example()
    }

    fn decode_named_dto(verb: AgentControlVerbV1, value: Value) -> serde_json::Result<()> {
        match verb {
            AgentControlVerbV1::ManagerInspect => serde_json::from_value::<
                crate::harness_manager_v2::AgentManagerInspectRequestV2,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerUpdate => serde_json::from_value::<
                crate::harness_manager_v2::AgentManagerUpdateRequestV2,
            >(value)
            .map(drop),
            AgentControlVerbV1::SubmitReviewReceipt => serde_json::from_value::<
                crate::harness_manager_v2::AgentSubmitReviewReceiptRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerControl => serde_json::from_value::<
                crate::harness_manager_v2::AgentManagerControlRequestV2,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerPrepareControl => serde_json::from_value::<
                crate::harness_manager_v2::AgentManagerPrepareControlRequestV2,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerCommitPreparedControl => serde_json::from_value::<
                crate::harness_manager_v2::AgentManagerCommitPreparedControlRequestV2,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerGetAction => serde_json::from_value::<
                crate::harness_manager_v2::AgentManagerGetActionRequestV2,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerLaunchIssueWorker => serde_json::from_value::<
                crate::manager_issue_worker::AgentManagerLaunchIssueWorkerRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::SpawnChild => {
                serde_json::from_value::<AgentSpawnChildRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ReserveSuccessor => {
                serde_json::from_value::<AgentReserveSuccessorRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::GetProgress => {
                serde_json::from_value::<AgentGetProgressParamsV1>(value).map(drop)
            }
            AgentControlVerbV1::SendMessage => {
                serde_json::from_value::<AgentSendMessageRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::GetStatus => {
                serde_json::from_value::<AgentGetStatusParams>(value).map(drop)
            }
            AgentControlVerbV1::Halt => serde_json::from_value::<AgentHaltParams>(value).map(drop),
            AgentControlVerbV1::ContinueChild => {
                serde_json::from_value::<AgentContinueChildRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ArchiveChild => {
                serde_json::from_value::<AgentArchiveChildRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ScheduleWake => {
                serde_json::from_value::<AgentScheduleWakeParams>(value).map(drop)
            }
            AgentControlVerbV1::CancelWake => {
                serde_json::from_value::<AgentCancelWakeParams>(value).map(drop)
            }
            AgentControlVerbV1::ListWakes => {
                serde_json::from_value::<AgentListWakesParams>(value).map(drop)
            }
            AgentControlVerbV1::CreateIssue => {
                serde_json::from_value::<AgentCreateIssueParams>(value).map(drop)
            }
            AgentControlVerbV1::ListIssues => {
                serde_json::from_value::<AgentListIssuesRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::GetIssue => {
                serde_json::from_value::<AgentGetIssueRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::UpdateIssue => {
                serde_json::from_value::<AgentUpdateIssueRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::UpdateIssueStatus => {
                serde_json::from_value::<AgentUpdateIssueStatusRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ArchiveIssue => {
                serde_json::from_value::<AgentArchiveIssueRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::RestoreIssue => {
                serde_json::from_value::<AgentRestoreIssueRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ListIssueEvents => {
                serde_json::from_value::<IssueEventPageRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerProgress => {
                serde_json::from_value::<AgentManagerProgressRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerInbox => {
                serde_json::from_value::<AgentManagerInboxRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerSend => {
                serde_json::from_value::<AgentManagerSendRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerReply => {
                serde_json::from_value::<AgentManagerReplyRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerNotify => {
                serde_json::from_value::<AgentManagerNotifyRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerWorkView => {
                serde_json::from_value::<AgentManagerWorkViewRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ManagerDelegateNode => {
                serde_json::from_value::<crate::manager_nodes::DelegateManagerNodeRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::ManagerEscalate => {
                serde_json::from_value::<crate::harness_manager::AgentManagerEscalateInputV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::ManagerListEscalations => serde_json::from_value::<
                crate::harness_manager::AgentManagerListEscalationsRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerResolveEscalation => serde_json::from_value::<
                crate::harness_manager::AgentManagerResolveEscalationRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::TopologyUpsert => {
                serde_json::from_value::<AgentTopologyUpsertRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::TopologyList => {
                serde_json::from_value::<AgentTopologyListRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::TopologyGet => {
                serde_json::from_value::<AgentTopologyGetRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::TopologyExecute => {
                serde_json::from_value::<AgentTopologyExecuteRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::TopologyGetExecution => {
                serde_json::from_value::<AgentTopologyGetExecutionRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::TopologyInterrupt => {
                serde_json::from_value::<AgentTopologyInterruptRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::TopologyResolveAttempt => {
                serde_json::from_value::<ResolveTopologyAttemptParams>(value).map(drop)
            }
            AgentControlVerbV1::GetAuthorityCatalog => serde_json::from_value::<
                crate::agent_authority_catalog::AgentGetAuthorityCatalogRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::SubmitJob => {
                serde_json::from_value::<crate::agent_jobs::AgentSubmitJobRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::GetJob => {
                serde_json::from_value::<crate::agent_jobs::AgentGetJobRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::ListJobs => {
                serde_json::from_value::<crate::agent_jobs::AgentListJobsRequestV1>(value).map(drop)
            }
            AgentControlVerbV1::CancelJob => {
                serde_json::from_value::<crate::agent_jobs::AgentCancelJobRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::EnqueueLandingSource => serde_json::from_value::<
                crate::rolling_queue::AgentEnqueueLandingSourceRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ListRollingQueue => serde_json::from_value::<
                crate::rolling_queue::AgentListRollingQueueRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::CancelRollingQueueEntry => serde_json::from_value::<
                crate::rolling_queue::AgentCancelRollingQueueEntryRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::CheckLandingCandidate => serde_json::from_value::<
                crate::agent_landing_candidate::AgentCheckLandingCandidateRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ReadSessionEvents => serde_json::from_value::<
                crate::agent_session_events::AgentReadSessionEventsRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::GetProviderStatus => serde_json::from_value::<
                crate::agent_provider_status::AgentGetProviderStatusRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::SendSatelliteMessage => serde_json::from_value::<
                crate::satellite_dispatch::AgentSendSatelliteMessageRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ReportToHub => serde_json::from_value::<
                crate::satellite_dispatch::AgentReportToHubRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::GetDaemonInfo => serde_json::from_value::<
                crate::agent_daemon_info::AgentGetDaemonInfoRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::QueryFailureSignatures => serde_json::from_value::<
                crate::agent_failure_signatures::AgentQueryFailureSignaturesRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::WorkerRoster => serde_json::from_value::<
                crate::agent_worker_roster::AgentWorkerRosterRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::PushSatelliteManagerPolicy => serde_json::from_value::<
                crate::satellite_manager_policy::AgentPushSatelliteManagerPolicyRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ReseatSatelliteManager => serde_json::from_value::<
                crate::manager_reseat::AgentReseatSatelliteManagerRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::RequestDeploy => {
                serde_json::from_value::<crate::agent_deploy::AgentRequestDeployRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::SwitchHostPreset => {
                serde_json::from_value::<crate::host_tuning::SwitchHostPreset>(value).map(drop)
            }
            AgentControlVerbV1::GlobalOverview => {
                serde_json::from_value::<crate::global_manager::AgentGlobalOverviewRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::GlobalSend => {
                serde_json::from_value::<crate::global_manager::AgentGlobalSendRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::GlobalAppointManager => serde_json::from_value::<
                crate::global_manager::AgentGlobalAppointManagerRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ReportToGlobal => {
                serde_json::from_value::<crate::global_manager::AgentReportToGlobalRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::ReportUp => {
                serde_json::from_value::<crate::manager_tier_routing::AgentReportUpRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::SendDown => {
                serde_json::from_value::<crate::manager_tier_routing::AgentSendDownRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::ManagerAppointChild => serde_json::from_value::<
                crate::portfolio_delegation::AgentManagerAppointChildRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerRevokeChild => serde_json::from_value::<
                crate::portfolio_delegation::AgentManagerRevokeChildRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::ManagerOverview => serde_json::from_value::<
                crate::manager_node_workspace::AgentManagerOverviewRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::CreateProject => {
                serde_json::from_value::<crate::agent_projects::AgentCreateProjectRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::UpdateProject => {
                serde_json::from_value::<crate::agent_projects::AgentUpdateProjectRequestV1>(value)
                    .map(drop)
            }
            AgentControlVerbV1::ListSourceWorktreeCohorts => serde_json::from_value::<
                crate::cohort_settlement::ListSourceWorktreeCohortsParams,
            >(value)
            .map(drop),
            AgentControlVerbV1::AuditSourceWorktreeCohort => serde_json::from_value::<
                crate::cohort_settlement::AgentAuditSourceWorktreeCohortParams,
            >(value)
            .map(drop),
            AgentControlVerbV1::ApplySourceWorktreeCohort => serde_json::from_value::<
                crate::cohort_settlement::ApplySourceWorktreeCohortParams,
            >(value)
            .map(drop),
            AgentControlVerbV1::OperatorTodoUpsert => serde_json::from_value::<
                crate::operator_todos::AgentOperatorTodoUpsertRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::OperatorTodoClose => serde_json::from_value::<
                crate::operator_todos::AgentOperatorTodoCloseRequestV1,
            >(value)
            .map(drop),
            AgentControlVerbV1::StandingDirective => serde_json::from_value::<
                crate::standing_directives::AgentStandingDirectiveRequestV1,
            >(value)
            .map(drop),
        }
    }

    #[test]
    fn catalog_is_closed_ordered_unique_and_valid() {
        let catalog = agent_control_catalog_v1();
        // No hand-pinned count or roster (#1116): the catalog is the single
        // declaration, so its length and order are not re-listed here. What
        // this test pins is shape: the authority catalog is served first, and
        // names are unique, `Agent`-prefixed and round-trip to their verb.
        assert_eq!(catalog[0].method, "AgentGetAuthorityCatalog");
        let names = catalog
            .iter()
            .map(|entry| entry.method)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), catalog.len());
        // Every v1 verb either maps to exactly one native in-process tool or
        // is explicitly declared RPC-only. The RPC-only set is pinned so a
        // future verb cannot silently omit its native mapping.
        let rpc_only = catalog
            .iter()
            .filter(|entry| entry.native_tool.is_none())
            .map(|entry| entry.method)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            rpc_only,
            BTreeSet::from([
                "AgentArchiveChild",
                "AgentReportToHub",
                "AgentPushSatelliteManagerPolicy",
                "AgentReseatSatelliteManager",
                "AgentSwitchHostPreset",
                "AgentGlobalAppointManager",
                "AgentManagerAppointChild",
                "AgentManagerRevokeChild",
            ]),
            "the RPC-only verb set changed without review"
        );
        let native = catalog
            .iter()
            .filter_map(|entry| entry.native_tool)
            .map(NativeAgentControlToolV1::name)
            .collect::<BTreeSet<_>>();
        assert_eq!(native.len(), catalog.len() - rpc_only.len());

        for descriptor in catalog {
            assert!(descriptor.method.starts_with("Agent"));
            assert_eq!(
                AgentControlVerbV1::from_method_name(descriptor.method),
                Some(descriptor.verb)
            );
            let schema = descriptor.parameters();
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
            assert_eq!(schema["additionalProperties"], false);
            let envelope: Value = serde_json::from_str(&descriptor.envelope_json()).unwrap();
            assert_eq!(envelope.as_object().unwrap().len(), 3);
            assert_eq!(envelope["schema_version"], AGENT_CONTROL_SCHEMA_VERSION_V1);
            assert_eq!(envelope["method"], descriptor.method);
            assert_eq!(envelope["parameters"], schema);
        }
        assert_eq!(
            AgentControlVerbV1::from_method_name("agentspawnchild"),
            None
        );
        assert_eq!(AgentControlVerbV1::from_method_name("GetSession"), None);
    }

    /// #3774: the manager verbs that used to be reachable only through
    /// `rsi-rpc` each have exactly one native tool, named for the verb, and
    /// the tool is served by the MCP gateway (no in-process handler).
    #[test]
    fn rpc_layer_verbs_have_gateway_native_tools_named_for_their_verb() {
        let expected = [
            ("AgentContinueChild", "rsi_control_continue_child"),
            ("AgentCancelWake", "rsi_control_cancel_wake"),
            (
                "AgentManagerDelegateNode",
                "rsi_control_manager_delegate_node",
            ),
            ("AgentManagerEscalate", "rsi_control_manager_escalate"),
            (
                "AgentManagerListEscalations",
                "rsi_control_manager_list_escalations",
            ),
            (
                "AgentManagerResolveEscalation",
                "rsi_control_manager_resolve_escalation",
            ),
            (
                "AgentEnqueueLandingSource",
                "rsi_control_enqueue_landing_source",
            ),
            ("AgentGetProviderStatus", "rsi_control_get_provider_status"),
            ("AgentSubmitJob", "rsi_control_submit_job"),
            ("AgentGetJob", "rsi_control_get_job"),
            ("AgentListJobs", "rsi_control_list_jobs"),
            ("AgentCancelJob", "rsi_control_cancel_job"),
            (
                "AgentSendSatelliteMessage",
                "rsi_control_send_satellite_message",
            ),
            ("AgentGetDaemonInfo", "rsi_control_get_daemon_info"),
            ("AgentRequestDeploy", "rsi_control_request_deploy"),
        ];
        let catalog = agent_control_catalog_v1();
        for (method, tool_name) in expected {
            let descriptor = catalog
                .iter()
                .find(|entry| entry.method == method)
                .unwrap_or_else(|| panic!("{method} is not catalogued"));
            let native = descriptor
                .native_tool
                .unwrap_or_else(|| panic!("{method} has no native tool"));
            assert_eq!(native.name(), tool_name);
            assert_eq!(native.verb(), descriptor.verb, "{tool_name}");
            assert!(native.mcp_gateway_only(), "{tool_name}");
        }
        let gateway_only = catalog
            .iter()
            .filter_map(|entry| entry.native_tool)
            .filter(|native| native.mcp_gateway_only())
            .count();
        assert_eq!(gateway_only, expected.len());
        // The same tool name resolves back to exactly one catalog verb.
        for descriptor in catalog {
            if let Some(native) = descriptor.native_tool {
                let owners = catalog
                    .iter()
                    .filter(|entry| entry.native_tool.map(|t| t.name()) == Some(native.name()))
                    .count();
                assert_eq!(owners, 1, "{}", native.name());
            }
        }
    }

    #[test]
    fn review_continuation_refusal_names_both_fields_and_the_fix() {
        let verb = AgentControlVerbV1::ManagerLaunchIssueWorker;
        let mut params = fixture(verb);
        params["sandbox_source"] = Value::Null;
        params["review_of"] = serde_json::json!(99);
        params["continue_from"] = serde_json::json!("00000000-0000-0000-0000-000000000001");
        let error = verb.validate_params(&params).unwrap_err();
        let mut envelope = serde_json::json!({});
        error.enrich(&mut envelope);
        assert_eq!(envelope["code"], "invalid_input");
        assert_eq!(envelope["path"], "params.continue_from");
        let constraint = envelope["constraint"].as_str().unwrap();
        assert!(constraint.contains("review_of and continue_from are mutually exclusive"));
        assert!(constraint.contains("omit continue_from"));

        // Each launch mode remains valid on its own, including explicit nulls.
        params["continue_from"] = Value::Null;
        assert_eq!(verb.validate_params(&params), Ok(()));
        params["continue_from"] = serde_json::json!("00000000-0000-0000-0000-000000000001");
        params["review_of"] = Value::Null;
        assert_eq!(verb.validate_params(&params), Ok(()));
    }

    #[test]
    fn submit_job_catalog_documents_and_validates_integration_test_targets() {
        let descriptor = agent_control_catalog_v1()
            .into_iter()
            .find(|entry| entry.method == "AgentSubmitJob")
            .unwrap();
        let schema = descriptor.parameters();
        assert!(
            schema["properties"]["params"]["description"]
                .as_str()
                .unwrap()
                .contains("test_target?")
        );
        assert!(descriptor.description.contains("test_target"));
        let valid = serde_json::json!({"kind":"test","params":{"package":"rsi-common","test_target":"manifest_dir_guard","exact":true,"filters":["tests::one"]}});
        assert!(descriptor.verb.validate_params(&valid).is_ok());
        for extra in [
            serde_json::json!({"test_target":"../guard"}),
            serde_json::json!({"test_target":"guard;id"}),
            serde_json::json!({"test_target":true}),
            serde_json::json!({"lib_only":true}),
            serde_json::json!({"exact":false}),
            serde_json::json!({"filters":[]}),
        ] {
            let mut invalid = valid.clone();
            invalid["params"]
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(
                descriptor.verb.validate_params(&invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn submit_job_catalog_exposes_the_rolling_tool_policy() {
        let descriptor = agent_control_catalog_v1()
            .into_iter()
            .find(|entry| entry.method == "AgentSubmitJob")
            .unwrap();
        let params = serde_json::json!({"kind":"test","params":{"scoped_test":{"base":"origin/rolling","rolling_tool":true}}});
        assert!(descriptor.verb.validate_params(&params).is_ok());
        let wire = descriptor.parameters().to_string();
        assert!(wire.contains("rolling_tool"));
        assert!(wire.contains("daemon-written"));
        let mut invalid = params;
        invalid["params"]["scoped_test"]["rolling_tool"] = serde_json::json!("true");
        assert!(descriptor.verb.validate_params(&invalid).is_err());
    }

    #[test]
    fn submit_job_catalog_documents_and_validates_package_exact_matching() {
        let descriptor = agent_control_catalog_v1()
            .into_iter()
            .find(|entry| entry.method == "AgentSubmitJob")
            .unwrap();
        let schema = descriptor.parameters();
        let params_text = schema["properties"]["params"]["description"]
            .as_str()
            .unwrap();
        assert!(params_text.contains("exact?:boolean"));
        assert!(params_text.contains("default false"));
        assert!(descriptor.description.contains("libtest --exact"));
        let value = serde_json::json!({"kind":"test","params":{"package":"rsi","lib_only":true,"filters":["module::tests::one"],"exact":true}});
        assert!(descriptor.verb.validate_params(&value).is_ok());
        let mut invalid = value;
        invalid["params"]["exact"] = serde_json::json!("true");
        assert!(descriptor.verb.validate_params(&invalid).is_err());
    }

    #[test]
    fn envelopes_are_byte_deterministic_and_have_fixed_key_order() {
        for descriptor in agent_control_catalog_v1() {
            let first = descriptor.envelope_json();
            let second = descriptor.envelope_json();
            assert_eq!(first, second);
            assert!(first.starts_with(&format!(
                "{{\"schema_version\":1,\"method\":\"{}\",\"parameters\":{{",
                descriptor.method
            )));
        }
    }

    #[test]
    fn fixtures_match_schema_fields_and_decode_into_all_named_dtos() {
        for descriptor in agent_control_catalog_v1() {
            let schema = descriptor.parameters();
            let fixture = fixture(descriptor.verb);
            let properties = schema["properties"].as_object().unwrap();
            let object = fixture.as_object().unwrap();
            for key in object.keys() {
                assert!(
                    properties.contains_key(key),
                    "{} fixture field {key} is absent from schema",
                    descriptor.method
                );
            }
            for required in schema["required"].as_array().into_iter().flatten() {
                let required = required.as_str().unwrap();
                assert!(
                    object.contains_key(required),
                    "{} fixture omits required field {required}",
                    descriptor.method
                );
            }
            decode_named_dto(descriptor.verb, fixture.clone()).unwrap();

            let mut identity_spoof = fixture;
            identity_spoof.as_object_mut().unwrap().insert(
                "caller_session_id".to_string(),
                Value::String("5d73c05d-1040-49f7-92ab-0123456789ab".to_string()),
            );
            let decoded = decode_named_dto(descriptor.verb, identity_spoof);
            if matches!(
                descriptor.verb,
                AgentControlVerbV1::GetStatus
                    | AgentControlVerbV1::Halt
                    | AgentControlVerbV1::ScheduleWake
            ) {
                assert!(
                    decoded.is_ok(),
                    "{} must retain its historical ignored-unknown RPC behavior",
                    descriptor.method
                );
            } else {
                assert!(
                    decoded.is_err(),
                    "{} strict DTO accepted a caller identity",
                    descriptor.method
                );
            }
        }
    }

    #[test]
    fn local_validation_accepts_fixtures_and_redacts_structural_failures() {
        for descriptor in agent_control_catalog_v1() {
            assert_eq!(
                descriptor.verb.validate_params(&fixture(descriptor.verb)),
                Ok(()),
                "{} fixture must validate",
                descriptor.method
            );
        }

        assert_eq!(
            AgentControlVerbV1::ListIssues.validate_params(&serde_json::json!({"ready":true})),
            Ok(())
        );
        assert!(
            AgentControlVerbV1::ListIssues
                .validate_params(&serde_json::json!({"ready":true,"extra":false}))
                .is_err()
        );
        assert!(
            AgentControlVerbV1::GetIssue
                .validate_params(&serde_json::json!({
                    "issue_id":"5d73c05d-1040-49f7-92ab-0123456789ab","extra":false
                }))
                .is_err()
        );

        let mut zero_fence = fixture(AgentControlVerbV1::ManagerControl);
        zero_fence["fence"]["scope_version"] = serde_json::json!(0);
        assert!(
            AgentControlVerbV1::ManagerControl
                .validate_params(&zero_fence)
                .is_err()
        );
        let oversized = serde_json::json!({
            "kind":"Task", "query":"x".repeat(262_145), "idempotency_key":"key"
        });
        assert!(
            AgentControlVerbV1::SpawnChild
                .validate_params(&oversized)
                .is_err()
        );
        let forged = serde_json::json!({"session_id":"5d73c05d-1040-49f7-92ab-0123456789ab","caller_session_id":"x"});
        assert!(
            AgentControlVerbV1::GetStatus
                .validate_params(&forged)
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fixed_schema_contract_fixtures_pin_all_verbs() {
        let shapes: &[(AgentControlVerbV1, &[&str], &[&str])] = &[
            (
                AgentControlVerbV1::GetAuthorityCatalog,
                &["fields", "verb"],
                &[],
            ),
            (
                AgentControlVerbV1::SpawnChild,
                &[
                    "agent_role",
                    "effort",
                    "idempotency_key",
                    "iteration",
                    "kind",
                    "model",
                    "provider",
                    "query",
                    "tags",
                    "topology_node",
                ],
                &["kind", "query", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ReserveSuccessor,
                &[
                    "effort",
                    "idempotency_key",
                    "iteration",
                    "kind",
                    "model",
                    "query",
                    "tags",
                    "topology_node",
                ],
                &["kind", "query", "idempotency_key"],
            ),
            (AgentControlVerbV1::GetProgress, &["session_ids"], &[]),
            (
                AgentControlVerbV1::SendMessage,
                &[
                    "expires_at",
                    "idempotency_key",
                    "message",
                    "session_id",
                    "target_session_id",
                ],
                &["message", "idempotency_key"],
            ),
            (AgentControlVerbV1::GetStatus, &["session_id"], &[]),
            (AgentControlVerbV1::Halt, &["session_id"], &[]),
            (
                AgentControlVerbV1::ContinueChild,
                &[
                    "expected_custody_generation",
                    "expected_event_sequence",
                    "expected_tip_session_id",
                    "query",
                    "session_id",
                    "target_session_id",
                ],
                &[
                    "query",
                    "expected_tip_session_id",
                    "expected_event_sequence",
                ],
            ),
            (
                AgentControlVerbV1::ArchiveChild,
                &[
                    "expected_custody_generation",
                    "expected_event_sequence",
                    "expected_tip_session_id",
                    "session_id",
                    "target_session_id",
                ],
                &["expected_tip_session_id", "expected_event_sequence"],
            ),
            (
                AgentControlVerbV1::ScheduleWake,
                &[
                    "at",
                    "every_seconds",
                    "in_seconds",
                    "message",
                    "mode",
                    "name",
                    "timeout_seconds",
                    "watch_session_id",
                    "when",
                ],
                &["message", "mode"],
            ),
            (AgentControlVerbV1::CancelWake, &["job_id", "name"], &[]),
            (
                AgentControlVerbV1::ListWakes,
                &["include_disabled", "limit", "session_id"],
                &[],
            ),
            (
                AgentControlVerbV1::CreateIssue,
                &[
                    "assignee",
                    "body",
                    "harness",
                    "idempotency_key",
                    "labels",
                    "priority",
                    "source_issue",
                    "title",
                ],
                &["title", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ListIssues,
                &[
                    "archive",
                    "cursor",
                    "fields",
                    "labels",
                    "limit",
                    "order",
                    "ready",
                    "status",
                    "title_contains",
                ],
                &[],
            ),
            (
                AgentControlVerbV1::GetIssue,
                &["display_number", "issue_id"],
                &[],
            ),
            (
                AgentControlVerbV1::UpdateIssue,
                &[
                    "assignee",
                    "body",
                    "clear_assignee",
                    "clear_priority",
                    "display_number",
                    "expected_row_version",
                    "handoff",
                    "idempotency_key",
                    "include_issue",
                    "issue_id",
                    "labels",
                    "priority",
                    "replace_body",
                    "title",
                ],
                &["expected_row_version", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::UpdateIssueStatus,
                &[
                    "display_number",
                    "expected_row_version",
                    "idempotency_key",
                    "include_issue",
                    "issue_id",
                    "note",
                    "status",
                ],
                &["status", "expected_row_version", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ArchiveIssue,
                &["expected_row_version", "idempotency_key", "issue_id"],
                &["issue_id", "expected_row_version", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::RestoreIssue,
                &["expected_row_version", "idempotency_key", "issue_id"],
                &["issue_id", "expected_row_version", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ListIssueEvents,
                &["after_sequence", "issue_id", "limit"],
                &["issue_id"],
            ),
            (
                AgentControlVerbV1::ManagerProgress,
                &["after_epic_id", "fields", "limit"],
                &[],
            ),
            (
                AgentControlVerbV1::ManagerInbox,
                &[
                    "after_notice_sequence",
                    "after_sequence",
                    "fields",
                    "include_sent",
                    "limit",
                    "notice_kind",
                    "read_notice_ids",
                    "request_id",
                    "settle_notice_ids",
                ],
                &[],
            ),
            (
                AgentControlVerbV1::ManagerSend,
                &["epic_id", "idempotency_key", "informational", "message"],
                &["epic_id", "message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerReply,
                &["idempotency_key", "message", "request_id", "still_running"],
                &["request_id", "message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerNotify,
                &["idempotency_key", "message"],
                &["message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerInspect,
                &["cursor", "epic_id", "include_history", "limit", "section"],
                &[],
            ),
            (
                AgentControlVerbV1::ManagerUpdate,
                &["change", "fence", "idempotency_key"],
                &["fence", "idempotency_key", "change"],
            ),
            (
                AgentControlVerbV1::SubmitReviewReceipt,
                &["assignment_id", "findings", "idempotency_key", "verdict"],
                &["verdict", "findings", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerControl,
                &["fence", "idempotency_key", "operation"],
                &["fence", "idempotency_key", "operation"],
            ),
            (
                AgentControlVerbV1::ManagerPrepareControl,
                &["operation"],
                &["operation"],
            ),
            (
                AgentControlVerbV1::ManagerCommitPreparedControl,
                &["idempotency_key", "prepared_id", "target_digest"],
                &["prepared_id", "target_digest", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerGetAction,
                &["operation_id"],
                &["operation_id"],
            ),
            (
                AgentControlVerbV1::ManagerLaunchIssueWorker,
                &[
                    "brief",
                    "continue_from",
                    "idempotency_key",
                    "issue",
                    "launch",
                    "parent_epic_id",
                    "qa_lane",
                    "related",
                    "review_of",
                    "sandbox_source",
                ],
                &["issue", "brief", "launch", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerWorkView,
                &["after_work_key", "limit", "work_key"],
                &[],
            ),
            (
                AgentControlVerbV1::ManagerDelegateNode,
                &[
                    "expected_node_grant_version",
                    "expected_parent_authority_epoch",
                    "expected_parent_grant_version",
                    "expected_parent_policy_version",
                    "grant",
                    "idempotency_key",
                    "node_id",
                    "parent_node_id",
                    "policy",
                    "seat_root_session_id",
                    "selector",
                ],
                &[
                    "expected_node_grant_version",
                    "expected_parent_authority_epoch",
                    "expected_parent_grant_version",
                    "expected_parent_policy_version",
                    "grant",
                    "idempotency_key",
                    "node_id",
                    "parent_node_id",
                    "policy",
                    "seat_root_session_id",
                    "selector",
                ],
            ),
            (
                AgentControlVerbV1::ManagerEscalate,
                &[
                    "expected_source_authority_epoch",
                    "expected_source_grant_version",
                    "expected_target_authority_epoch",
                    "expected_target_grant_version",
                    "expected_target_session_id",
                    "idempotency_key",
                    "reason",
                    "route",
                    "subject_id",
                ],
                &[
                    "expected_source_authority_epoch",
                    "expected_source_grant_version",
                    "expected_target_authority_epoch",
                    "expected_target_grant_version",
                    "expected_target_session_id",
                    "idempotency_key",
                    "reason",
                    "route",
                    "subject_id",
                ],
            ),
            (AgentControlVerbV1::ManagerListEscalations, &[], &[]),
            (
                AgentControlVerbV1::ManagerResolveEscalation,
                &[
                    "escalation_id",
                    "expected_target_authority_epoch",
                    "expected_target_grant_version",
                    "expected_target_session_id",
                    "expected_version",
                    "idempotency_key",
                    "ruling",
                ],
                &[
                    "escalation_id",
                    "expected_target_authority_epoch",
                    "expected_target_grant_version",
                    "expected_target_session_id",
                    "expected_version",
                    "idempotency_key",
                    "ruling",
                ],
            ),
            (
                AgentControlVerbV1::TopologyUpsert,
                &[
                    "definition",
                    "epic_id",
                    "expected_revision",
                    "idempotency_key",
                    "name",
                    "scope",
                    "validate_only",
                ],
                &["name", "definition", "scope", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::TopologyList,
                &[
                    "cursor",
                    "epic_id",
                    "include_executions",
                    "include_settled",
                    "limit",
                    "scope",
                ],
                &[],
            ),
            (
                AgentControlVerbV1::TopologyGet,
                &["name", "topology_id"],
                &[],
            ),
            (
                AgentControlVerbV1::TopologyExecute,
                &[
                    "base_commit",
                    "epic_id",
                    "expected_digest",
                    "idempotency_key",
                    "inputs",
                    "on_call",
                    "supersedes_execution",
                    "topology_id",
                    "wake",
                ],
                &[
                    "topology_id",
                    "expected_digest",
                    "epic_id",
                    "idempotency_key",
                ],
            ),
            (
                AgentControlVerbV1::TopologyGetExecution,
                &["after_sequence", "execution_id", "limit"],
                &["execution_id"],
            ),
            (
                AgentControlVerbV1::TopologyInterrupt,
                &["execution_id", "expected_row_version", "idempotency_key"],
                &["execution_id", "expected_row_version", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::SubmitJob,
                &[
                    "idempotency_key",
                    "kind",
                    "name",
                    "params",
                    "sandbox_session_id",
                    "source_session_id",
                    "wake",
                    "worktree",
                ],
                &["kind", "params"],
            ),
            (
                AgentControlVerbV1::GetJob,
                &["fields", "job_id"],
                &["job_id"],
            ),
            (AgentControlVerbV1::ListJobs, &["fields", "limit"], &[]),
            (AgentControlVerbV1::CancelJob, &["job_id"], &["job_id"]),
            (
                AgentControlVerbV1::EnqueueLandingSource,
                &[
                    "derive_filters",
                    "expedite",
                    "fixes_issue",
                    "fixes_tests",
                    "idempotency_key",
                    "issue",
                    "source_commit",
                    "source_session_id",
                    "supersedes",
                    "test_filters",
                ],
                &["source_commit", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ListRollingQueue,
                &["before_sequence", "fields", "limit", "state"],
                &[],
            ),
            (
                AgentControlVerbV1::CancelRollingQueueEntry,
                &["entry_id", "reason"],
                &["entry_id"],
            ),
            (
                AgentControlVerbV1::CheckLandingCandidate,
                &["job_id", "source_commit", "source_session_id"],
                &["source_commit"],
            ),
            (
                AgentControlVerbV1::GetProviderStatus,
                &["models", "provider", "run_budget_usd", "run_sessions"],
                &[],
            ),
            (AgentControlVerbV1::GetDaemonInfo, &[], &[]),
            (
                AgentControlVerbV1::QueryFailureSignatures,
                &["digest", "test_id"],
                &[],
            ),
            (
                AgentControlVerbV1::WorkerRoster,
                &["fields", "limit", "states"],
                &[],
            ),
            (AgentControlVerbV1::GlobalOverview, &[], &[]),
            (
                AgentControlVerbV1::ManagerOverview,
                &["detail", "directives", "fleet"],
                &[],
            ),
            (
                AgentControlVerbV1::CreateProject,
                &["color", "description", "name", "path"],
                &["name", "path"],
            ),
            (
                AgentControlVerbV1::UpdateProject,
                &["color", "description", "name", "path", "project_id"],
                &["project_id"],
            ),
            (
                AgentControlVerbV1::OperatorTodoUpsert,
                &[
                    "how",
                    "idempotency_key",
                    "link",
                    "priority",
                    "title",
                    "todo_key",
                ],
                &["todo_key", "title", "how", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::OperatorTodoClose,
                &[
                    "evidence",
                    "idempotency_key",
                    "observed",
                    "status",
                    "todo_id",
                    "todo_key",
                ],
                &["status", "evidence", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::StandingDirective,
                &[
                    "action",
                    "directive_id",
                    "expires_in_secs",
                    "idempotency_key",
                    "kind",
                    "provenance",
                    "scope",
                    "text",
                ],
                &["action"],
            ),
            (
                AgentControlVerbV1::GlobalSend,
                &["idempotency_key", "message", "project_id"],
                &["project_id", "message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::GlobalAppointManager,
                &[
                    "idempotency_key",
                    "launch",
                    "project_id",
                    "query",
                    "sandbox",
                ],
                &["project_id", "launch", "query", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ReportToGlobal,
                &["idempotency_key", "message"],
                &["message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ReportUp,
                &["idempotency_key", "message"],
                &["message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::SendDown,
                &["idempotency_key", "message", "target"],
                &["target", "message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerAppointChild,
                &[
                    "handoff",
                    "idempotency_key",
                    "launch",
                    "query",
                    "sandbox",
                    "target",
                ],
                &["target", "launch", "query", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerRevokeChild,
                &["expected_grant_version", "idempotency_key", "node_id"],
                &["node_id", "expected_grant_version", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::PushSatelliteManagerPolicy,
                &[
                    "expected_policy_version",
                    "expected_scope_version",
                    "idempotency_key",
                    "peer_id",
                    "policy",
                    "remote_session_id",
                ],
                &[
                    "peer_id",
                    "remote_session_id",
                    "expected_scope_version",
                    "expected_policy_version",
                    "idempotency_key",
                    "policy",
                ],
            ),
            (
                AgentControlVerbV1::ReseatSatelliteManager,
                &[
                    "effort",
                    "model",
                    "peer_id",
                    "provider",
                    "provider_unavailable",
                    "reason",
                    "remote_session_id",
                ],
                &["peer_id", "remote_session_id", "provider", "reason"],
            ),
            (
                AgentControlVerbV1::RequestDeploy,
                &[
                    "binaries_dir",
                    "build",
                    "cancel",
                    "idempotency_key",
                    "interrupt_workers",
                    "max_wait_secs",
                    "peer_id",
                    "sha",
                ],
                &["sha", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ReportToHub,
                &["binaries_dir", "kind", "text"],
                &["kind", "text"],
            ),
            (
                AgentControlVerbV1::SwitchHostPreset,
                &["action", "name"],
                &["action"],
            ),
            (
                AgentControlVerbV1::SendSatelliteMessage,
                &[
                    "expires_at",
                    "idempotency_key",
                    "issue_refs",
                    "message",
                    "peer_id",
                    "remote_session_id",
                    "scope_project_id",
                ],
                &["peer_id", "remote_session_id", "message", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::TopologyResolveAttempt,
                &[
                    "action",
                    "attempt_id",
                    "confirm_preserved_commit",
                    "execution_id",
                    "expected_row_version",
                    "idempotency_key",
                ],
                &[
                    "execution_id",
                    "attempt_id",
                    "action",
                    "expected_row_version",
                    "idempotency_key",
                ],
            ),
            (AgentControlVerbV1::ListSourceWorktreeCohorts, &[], &[]),
            (
                AgentControlVerbV1::AuditSourceWorktreeCohort,
                &["after_custody_id", "fields", "repository_identity"],
                &["repository_identity"],
            ),
            (
                AgentControlVerbV1::ApplySourceWorktreeCohort,
                &[
                    "after_custody_id",
                    "authorization",
                    "idempotency_key",
                    "plan_digest",
                    "repository_identity",
                ],
                &[
                    "repository_identity",
                    "plan_digest",
                    "authorization",
                    "idempotency_key",
                ],
            ),
            (
                AgentControlVerbV1::ReadSessionEvents,
                &[
                    "after_sequence",
                    "event_types",
                    "final_message_full",
                    "full_fields",
                    "limit",
                    "max_bytes",
                    "session_id",
                ],
                &["session_id"],
            ),
        ];

        assert_eq!(shapes.len(), agent_control_catalog_v1().len());

        for (verb, expected_properties, expected_required) in shapes {
            let schema = verb.descriptor().parameters();
            let mut properties = schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            properties.sort_unstable();
            // #1235: every project-bound verb also names the optional target.
            if verb.is_project_bound() {
                let at = properties
                    .iter()
                    .position(|property| *property == "project_id")
                    .unwrap_or_else(|| panic!("{verb:?} must name project_id"));
                properties.remove(at);
            }
            assert_eq!(properties, *expected_properties, "verb: {verb:?}");
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|fields| fields.iter().map(|field| field.as_str().unwrap()).collect())
                .unwrap_or_default();
            assert_eq!(required, *expected_required, "verb: {verb:?}");
        }

        let spawn = AgentControlVerbV1::SpawnChild.descriptor().parameters();
        assert_eq!(
            spawn["properties"]["kind"]["enum"],
            serde_json::json!(["Story", "Task", "Bug", "Feature", "Refactor", "Research"])
        );
        assert_eq!(
            spawn["properties"]["provider"]["enum"],
            serde_json::json!([
                "Claude",
                "Codex",
                "Pioneer",
                "OpenRouter",
                "Bedrock",
                "Local",
                "Antigravity",
                "CodexAppServer",
                "Harness",
                "Gemini",
                null
            ])
        );
        assert_eq!(spawn["properties"]["query"]["minLength"], 1);
        assert_eq!(spawn["properties"]["query"]["maxLength"], 262_144);
        assert_eq!(spawn["properties"]["iteration"]["maximum"], u32::MAX);
        assert_eq!(spawn["properties"]["tags"]["maxItems"], 64);
        assert_eq!(spawn["properties"]["idempotency_key"]["maxLength"], 128);

        let successor = AgentControlVerbV1::ReserveSuccessor
            .descriptor()
            .parameters();
        assert!(successor["properties"].get("provider").is_none());
        assert_eq!(successor["properties"]["query"]["maxLength"], 262_144);
        assert_eq!(successor["properties"]["tags"]["maxItems"], 64);

        let progress = AgentControlVerbV1::GetProgress.descriptor().parameters();
        assert_eq!(progress["properties"]["session_ids"]["maxItems"], 256);
        assert_eq!(
            progress["properties"]["session_ids"]["default"],
            serde_json::json!([])
        );
        assert_eq!(
            progress["properties"]["session_ids"]["items"]["pattern"],
            crate::agent_id_ref::ID_REF_PATTERN
        );

        let message = AgentControlVerbV1::SendMessage.descriptor().parameters();
        assert_eq!(message["properties"]["message"]["minLength"], 1);
        assert_eq!(message["properties"]["message"]["maxLength"], 16_384);
        assert_eq!(
            message["properties"]["expires_at"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(message["properties"]["expires_at"]["format"], "date-time");
        assert!(
            message["properties"]["expires_at"]["description"]
                .as_str()
                .unwrap()
                .contains("30 minutes after first acceptance")
        );
        assert!(
            message["properties"]["message"]["description"]
                .as_str()
                .unwrap()
                .contains("acceptance does not prove delivery")
        );

        for verb in [AgentControlVerbV1::GetStatus, AgentControlVerbV1::Halt] {
            let target = verb.descriptor().parameters();
            assert_eq!(
                target["properties"]["session_id"]["type"],
                serde_json::json!(["string", "null"])
            );
            assert_eq!(target["properties"]["session_id"]["default"], Value::Null);
            if verb == AgentControlVerbV1::GetStatus {
                // #2312: a read verb takes a unique id prefix too.
                assert_eq!(
                    target["properties"]["session_id"]["pattern"],
                    crate::agent_id_ref::ID_REF_PATTERN
                );
            } else {
                assert_eq!(target["properties"]["session_id"]["format"], "uuid");
            }
        }

        let wake = AgentControlVerbV1::ScheduleWake.descriptor().parameters();
        assert_eq!(
            wake["properties"]["mode"]["enum"],
            serde_json::json!(["fresh", "resume", "on_terminal", "program_guard", "when"])
        );
        // #2261: the topology execution predicate is listed beside the others.
        let predicates = wake["properties"]["when"]["properties"]
            .as_object()
            .expect("when predicates");
        for name in [
            "jobs_terminal",
            "sha_on_rolling",
            "shas_on_rolling",
            "issue_closed",
            "todo_closed",
            "topology_execution_settled",
            "settlement_run_settled",
            "health_anomaly",
        ] {
            assert!(predicates.contains_key(name), "{name}");
        }
        assert_eq!(predicates["shas_on_rolling"]["minItems"], 1);
        assert_eq!(
            predicates["shas_on_rolling"]["maxItems"],
            crate::wake_predicate::WAKE_WHEN_MAX_SHAS
        );
        assert_eq!(predicates["shas_on_rolling"]["uniqueItems"], true);
        // #217: the per-landing firing mode and the predicate-wake cap.
        assert_eq!(
            predicates["fire"]["enum"],
            serde_json::json!(["all", "each"])
        );
        let when_description = wake["properties"]["when"]["description"].as_str().unwrap();
        assert!(when_description.contains(crate::wake_predicate::WAKE_WHEN_CAP_REACHED));
        assert!(when_description.contains(&format!(
            "at most {} enabled when wakes",
            crate::wake_predicate::WAKE_WHEN_MAX_PER_SESSION
        )));
        assert_eq!(predicates["topology_execution_settled"]["format"], "uuid");
        assert_eq!(predicates["settlement_run_settled"]["format"], "uuid");
        assert!(
            wake["properties"]["when"]["description"]
                .as_str()
                .unwrap()
                .contains("topology_execution_settled")
        );
        assert_eq!(wake["properties"]["in_seconds"]["minimum"], 1);
        assert_eq!(wake["properties"]["every_seconds"]["minimum"], 1);
        assert_eq!(wake["properties"]["at"]["format"], "date-time");
        assert_eq!(wake["properties"]["watch_session_id"]["format"], "uuid");

        let create = AgentControlVerbV1::CreateIssue.descriptor().parameters();
        assert_eq!(create["properties"]["title"]["maxLength"], 512);
        assert_eq!(create["properties"]["body"]["default"], "");
        assert_eq!(create["properties"]["body"]["maxLength"], 65_536);
        assert_eq!(create["properties"]["priority"]["minimum"], 1);
        assert_eq!(create["properties"]["priority"]["maximum"], 4);
        assert_eq!(
            create["properties"]["labels"]["default"],
            serde_json::json!([])
        );
        assert_eq!(create["properties"]["labels"]["maxItems"], 64);

        let list = AgentControlVerbV1::ListIssues.descriptor().parameters();
        assert_eq!(
            list["properties"]["status"]["enum"],
            serde_json::json!(["Open", "InProgress", "Closed", "Cancelled", null])
        );
        assert_eq!(
            list["properties"]["archive"]["enum"],
            serde_json::json!(["Active", "Archived", "All"])
        );
        assert_eq!(list["properties"]["archive"]["default"], "Active");
        assert_eq!(list["properties"]["cursor"]["additionalProperties"], false);
        assert_eq!(
            list["properties"]["cursor"]["required"],
            serde_json::json!(["display_number", "issue_id"])
        );
        assert_eq!(list["properties"]["limit"]["default"], 64);
        assert_eq!(list["properties"]["limit"]["maximum"], 256);

        let get = AgentControlVerbV1::GetIssue.descriptor().parameters();
        assert_eq!(get["properties"]["issue_id"]["format"], "uuid");

        let update = AgentControlVerbV1::UpdateIssue.descriptor().parameters();
        assert_eq!(update["properties"]["expected_row_version"]["minimum"], 1);
        assert_eq!(
            update["properties"]["title"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(update["properties"]["title"]["maxLength"], 512);
        assert_eq!(update["properties"]["body"]["maxLength"], 65_536);
        assert_eq!(update["properties"]["handoff"]["maxLength"], 65_536);
        assert_eq!(update["properties"]["labels"]["maxItems"], 64);
        assert_eq!(update["properties"]["priority"]["maximum"], 4);
        assert_eq!(update["properties"]["clear_priority"]["default"], false);
        assert_eq!(update["properties"]["clear_assignee"]["default"], false);
        assert_eq!(update["properties"]["include_issue"]["default"], false);

        let status = AgentControlVerbV1::UpdateIssueStatus
            .descriptor()
            .parameters();
        assert_eq!(
            status["properties"]["status"]["enum"],
            serde_json::json!(["Open", "InProgress", "Closed", "Cancelled"])
        );
        assert_eq!(status["properties"]["expected_row_version"]["minimum"], 1);
        assert_eq!(status["properties"]["include_issue"]["default"], false);

        for verb in [
            AgentControlVerbV1::ArchiveIssue,
            AgentControlVerbV1::RestoreIssue,
        ] {
            let cas = verb.descriptor().parameters();
            assert_eq!(cas["properties"]["expected_row_version"]["minimum"], 1);
            assert_eq!(cas["properties"]["idempotency_key"]["maxLength"], 128);
        }

        let events = AgentControlVerbV1::ListIssueEvents
            .descriptor()
            .parameters();
        assert_eq!(events["properties"]["after_sequence"]["minimum"], 0);
        assert_eq!(events["properties"]["after_sequence"]["default"], 0);
        assert_eq!(events["properties"]["limit"]["default"], 64);
        assert_eq!(events["properties"]["limit"]["maximum"], 256);
    }

    #[test]
    fn identity_and_operator_fields_are_absent_from_every_schema() {
        let forbidden = [
            "token",
            "session_token",
            "caller_session_id",
            "sender_session_id",
            "created_by_session_id",
            "owner_session_id",
            "origin_session_id",
            "parent_id",
            "predecessor_session_id",
            "project_id",
            "epic_id",
            "epic_spawn_ordinal",
            "recipient_session_id",
            "manager_session_id",
            "lead_session_id",
            "authority",
            "lead_generation",
            "generation",
            "wake_session_id",
            "program_id",
            "program_run_id",
            "controller_epoch",
            "lease_generation",
            "claim_generation",
            "expected_run_version",
            "expected_idea_version",
        ];
        for descriptor in agent_control_catalog_v1() {
            let schema = descriptor.parameters();
            let encoded = schema.to_string();
            // Manager send names a routing target, never the caller's owning Epic.
            // Topology verbs name a target Epic that the daemon checks
            // against the caller's live scope.
            assert_eq!(
                schema["properties"].get("epic_id").is_some(),
                matches!(
                    descriptor.verb,
                    AgentControlVerbV1::ManagerSend
                        | AgentControlVerbV1::ManagerInspect
                        | AgentControlVerbV1::TopologyUpsert
                        | AgentControlVerbV1::TopologyList
                        | AgentControlVerbV1::TopologyExecute
                )
            );
            for field in forbidden {
                // Manager operations name an authorized routing target; the
                // field never supplies the caller's owning Epic identity.
                if field == "epic_id"
                    && matches!(
                        descriptor.verb,
                        AgentControlVerbV1::ManagerSend
                            | AgentControlVerbV1::ManagerInspect
                            | AgentControlVerbV1::ManagerUpdate
                            | AgentControlVerbV1::ManagerControl
                            | AgentControlVerbV1::ManagerPrepareControl
                            | AgentControlVerbV1::TopologyUpsert
                            | AgentControlVerbV1::TopologyList
                            | AgentControlVerbV1::TopologyExecute
                    )
                {
                    continue;
                }
                // #872: the global seat names a target project inside its
                // operator grant; the daemon checks it against the grant and
                // never reads it as the caller's own project.
                if field == "project_id"
                    && matches!(
                        descriptor.verb,
                        AgentControlVerbV1::GlobalSend
                            | AgentControlVerbV1::GlobalAppointManager
                            // #1239: a covered project to seat a PM in.
                            | AgentControlVerbV1::ManagerAppointChild
                            // #1626: the project to edit, inside the caller's coverage.
                            | AgentControlVerbV1::UpdateProject
                    )
                {
                    continue;
                }
                // #1238: `AgentSendDown` names a descendant node as its target;
                // the daemon checks it against the caller's own node and never
                // reads it as the caller's identity.
                if field == "project_id" && descriptor.verb == AgentControlVerbV1::SendDown {
                    assert!(schema["properties"].get(field).is_none());
                    continue;
                }
                // #1235: the project-bound PM verb set names an optional
                // target project; the daemon resolves it against the caller's
                // own project or the global seat's grant, never as identity.
                if field == "project_id" && descriptor.verb.is_project_bound() {
                    assert_eq!(
                        schema["properties"]["project_id"]["default"],
                        Value::Null,
                        "{} project_id must be optional",
                        descriptor.method
                    );
                    continue;
                }
                // V2 control names authorized parents and observed lead fences;
                // these are scoped operation data, never caller credentials.
                if matches!(
                    descriptor.verb,
                    AgentControlVerbV1::ManagerControl | AgentControlVerbV1::ManagerPrepareControl
                ) && ["parent_id", "lead_session_id", "lead_generation"].contains(&field)
                {
                    assert!(schema["properties"].get(field).is_none());
                    continue;
                }
                assert!(
                    !encoded.contains(&format!("\"{field}\"")),
                    "{} exposed forbidden field {field}",
                    descriptor.method
                );
            }
        }
    }

    #[test]
    fn manager_catalog_matches_dto_defaults_and_runtime_byte_bounds() {
        let inbox = AgentControlVerbV1::ManagerInbox.descriptor().parameters();
        let defaults: AgentManagerInboxRequestV1 =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(inbox["properties"]["after_sequence"]["minimum"], 0);
        assert_eq!(
            inbox["properties"]["after_sequence"]["default"],
            defaults.after_sequence
        );
        assert_eq!(inbox["properties"]["limit"]["default"], defaults.limit);
        assert_eq!(inbox["properties"]["limit"]["minimum"], 1);
        assert_eq!(
            inbox["properties"]["limit"]["maximum"],
            HARNESS_MANAGER_MAX_INBOX_PAGE
        );
        assert_eq!(inbox["properties"]["request_id"]["format"], "uuid");
        assert_eq!(defaults.request_id, None);
        assert!(!defaults.include_sent);
        assert_eq!(inbox["properties"]["include_sent"]["type"], "boolean");
        assert_eq!(inbox["properties"]["include_sent"]["default"], false);
        assert_eq!(
            inbox["properties"]["after_notice_sequence"]["default"],
            defaults.after_notice_sequence
        );
        assert_eq!(inbox["properties"]["after_notice_sequence"]["minimum"], 0);
        assert_eq!(
            inbox["properties"]["settle_notice_ids"]["maxItems"],
            HARNESS_MANAGER_MAX_INBOX_PAGE
        );
        assert_eq!(
            inbox["properties"]["settle_notice_ids"]["items"]["format"],
            "uuid"
        );
        assert_eq!(
            inbox["properties"]["read_notice_ids"]["maxItems"],
            HARNESS_MANAGER_MAX_INBOX_PAGE
        );
        assert_eq!(
            inbox["properties"]["read_notice_ids"]["items"]["format"],
            "uuid"
        );
        assert!(defaults.read_notice_ids.is_empty());
        assert_eq!(inbox["properties"]["notice_kind"]["default"], Value::Null);
        assert_eq!(inbox["properties"]["fields"]["default"], "compact");
        assert_eq!(
            inbox["properties"]["fields"]["enum"],
            serde_json::json!(["compact", "full", "summary"])
        );
        assert_eq!(
            defaults.fields,
            crate::harness_manager::AgentManagerInboxFieldsV1::Compact
        );
        for limit in [0, HARNESS_MANAGER_MAX_INBOX_PAGE + 1] {
            assert!(
                AgentManagerInboxRequestV1 {
                    limit,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        for verb in [
            AgentControlVerbV1::ManagerSend,
            AgentControlVerbV1::ManagerReply,
            AgentControlVerbV1::ManagerNotify,
        ] {
            let schema = verb.descriptor().parameters();
            assert_eq!(
                schema["properties"]["message"]["maxLength"],
                HARNESS_MANAGER_MAX_MESSAGE_BYTES
            );
            assert_eq!(schema["properties"]["idempotency_key"]["maxLength"], 128);
        }
        let id = Uuid::new_v4();
        assert!(
            validate_manager_message(
                id,
                &"é".repeat(HARNESS_MANAGER_MAX_MESSAGE_BYTES / 2),
                "one"
            )
            .is_ok()
        );
        assert!(
            validate_manager_message(
                id,
                &"é".repeat(HARNESS_MANAGER_MAX_MESSAGE_BYTES / 2 + 1),
                "one"
            )
            .is_err()
        );
        for method in ["GetHarnessManager", "ConfigureHarnessManager"] {
            assert_eq!(AgentControlVerbV1::from_method_name(method), None);
        }
    }

    /// Issue #548: page selection only; caller, Epic, manager and scope are
    /// daemon-derived, so spoofed identity fields are refused by both the
    /// schema validator and the DTO.
    #[test]
    fn work_view_schema_matches_dto_defaults_and_refuses_identity_fields() {
        let verb = AgentControlVerbV1::ManagerWorkView;
        assert_eq!(
            AgentControlVerbV1::from_method_name("AgentManagerWorkView"),
            Some(verb)
        );
        assert_eq!(
            verb.descriptor()
                .native_tool
                .map(NativeAgentControlToolV1::name),
            Some("rsi_control_manager_work_view")
        );
        let schema = verb.descriptor().parameters();
        let defaults: AgentManagerWorkViewRequestV1 =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(defaults, AgentManagerWorkViewRequestV1::default());
        assert_eq!(schema["properties"]["limit"]["default"], defaults.limit);
        assert_eq!(
            schema["properties"]["limit"]["maximum"],
            crate::harness_manager::MANAGER_WORK_VIEW_MAX_PAGE
        );
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        for key in ["work_key", "after_work_key"] {
            assert_eq!(schema["properties"][key]["maxLength"], 256);
            assert_eq!(schema["properties"][key]["default"], Value::Null);
        }
        assert!(verb.validate_params(&serde_json::json!({})).is_ok());
        assert!(
            verb.validate_params(&serde_json::json!({"work_key":"alpha","limit":32}))
                .is_ok()
        );
        for invalid in [
            serde_json::json!({"limit": 0}),
            serde_json::json!({"limit": 33}),
            serde_json::json!({"work_key": ""}),
            serde_json::json!({"after_work_key": "k".repeat(257)}),
        ] {
            assert!(verb.validate_params(&invalid).is_err(), "{invalid}");
        }
        let id = Uuid::new_v4();
        for field in [
            "epic_id",
            "caller_session_id",
            "session_id",
            "manager_session_id",
            "scope_version",
            "project_id",
        ] {
            let spoof = serde_json::json!({ field: id });
            assert!(verb.validate_params(&spoof).is_err(), "{field}");
            assert!(
                serde_json::from_value::<AgentManagerWorkViewRequestV1>(spoof).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn notify_schema_accepts_only_message_and_key() {
        let descriptor = AgentControlVerbV1::ManagerNotify.descriptor();
        let schema = descriptor.parameters();
        assert_eq!(schema["additionalProperties"], false);
        let keys = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(keys, BTreeSet::from(["idempotency_key", "message"]));
        let valid = serde_json::json!({"message":"status: ready","idempotency_key":"one"});
        assert!(
            AgentControlVerbV1::ManagerNotify
                .validate_params(&valid)
                .is_ok()
        );
        let dto: AgentManagerNotifyRequestV1 = serde_json::from_value(valid.clone()).unwrap();
        assert!(dto.validate().is_ok());
        for field in ["epic_id", "caller_session_id", "sender_session_id"] {
            let mut forged = valid.clone();
            forged[field] = serde_json::json!(Uuid::new_v4());
            assert!(
                AgentControlVerbV1::ManagerNotify
                    .validate_params(&forged)
                    .is_err()
            );
            assert!(serde_json::from_value::<AgentManagerNotifyRequestV1>(forged).is_err());
        }
    }

    /// Issue #633 (T4-A7): the six scoped topology verbs are catalogued with
    /// native tools, the operator topology family stays out of the agent
    /// catalog, and the resolution schema carries the discard confirmation
    /// rule (plan §3.4, R4-1).
    #[test]
    fn topology_verbs_are_catalogued_and_operator_topology_verbs_stay_operator_only() {
        let topology = agent_control_catalog_v1()
            .iter()
            .filter(|descriptor| descriptor.method.starts_with("AgentTopology"))
            .map(|descriptor| {
                (
                    descriptor.method,
                    descriptor.native_tool.map(NativeAgentControlToolV1::name),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            topology,
            [
                ("AgentTopologyUpsert", Some("rsi_control_topology_upsert")),
                ("AgentTopologyList", Some("rsi_control_topology_list")),
                ("AgentTopologyGet", Some("rsi_control_topology_get")),
                ("AgentTopologyExecute", Some("rsi_control_topology_execute")),
                (
                    "AgentTopologyGetExecution",
                    Some("rsi_control_topology_get_execution")
                ),
                (
                    "AgentTopologyInterrupt",
                    Some("rsi_control_topology_interrupt")
                ),
                (
                    "AgentTopologyResolveAttempt",
                    Some("rsi_control_topology_resolve_attempt")
                ),
            ]
        );
        for operator in [
            "CreateTopology",
            "UpdateTopology",
            "ListTopologies",
            "ExecuteTopology",
            "GetWorkflowExecution",
            "InterruptWorkflowExecution",
            "ResolveTopologyAttempt",
        ] {
            assert_eq!(AgentControlVerbV1::from_method_name(operator), None);
        }

        let resolve = AgentControlVerbV1::TopologyResolveAttempt;
        let id = "5d73c05d-1040-49f7-92ab-0123456789ab";
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let request = |action: &str, confirm: Option<&str>| {
            serde_json::json!({
                "execution_id": id, "attempt_id": id, "action": action,
                "expected_row_version": 1, "idempotency_key": "k",
                "confirm_preserved_commit": confirm,
            })
        };
        assert_eq!(resolve.validate_params(&request("inspect", None)), Ok(()));
        assert_eq!(
            resolve.validate_params(&request("discard", Some(commit))),
            Ok(())
        );
        for invalid in [
            request("discard", None),
            request("discard", Some("abc")),
            request("retry", Some(commit)),
        ] {
            assert!(resolve.validate_params(&invalid).is_err(), "{invalid}");
        }
        let execute = AgentControlVerbV1::TopologyExecute;
        let mut spoof = fixture(execute);
        spoof["requested_by_session_id"] = serde_json::json!(id);
        assert!(execute.validate_params(&spoof).is_err());
        let mut digest = fixture(execute);
        digest["expected_digest"] = serde_json::json!("sha256:ABC");
        assert!(execute.validate_params(&digest).is_err());
        assert_eq!(
            AgentControlVerbV1::TopologyList.validate_params(&serde_json::json!({})),
            Ok(())
        );
        assert!(
            AgentControlVerbV1::TopologyList
                .validate_params(&serde_json::json!({"limit": 33}))
                .is_err()
        );
    }

    #[test]
    fn runtime_only_predicates_are_not_overpromised() {
        let wake = AgentControlVerbV1::ScheduleWake.descriptor().parameters();
        assert!(wake.get("oneOf").is_none());
        assert!(wake.get("if").is_none());
        let update = AgentControlVerbV1::UpdateIssue.descriptor().parameters();
        assert!(update.get("anyOf").is_none());
        let issue_id =
            &AgentControlVerbV1::GetIssue.descriptor().parameters()["properties"]["issue_id"];
        assert_eq!(issue_id["format"], "uuid");
        assert!(issue_id.get("not").is_none());
    }

    #[test]
    fn issue_defaults_and_bounds_are_explicit() {
        let list = AgentControlVerbV1::ListIssues.descriptor().parameters();
        assert_eq!(list["properties"]["archive"]["default"], "Active");
        assert_eq!(list["properties"]["limit"]["default"], 64);
        assert_eq!(list["properties"]["limit"]["maximum"], 256);
        let events = AgentControlVerbV1::ListIssueEvents
            .descriptor()
            .parameters();
        assert_eq!(events["properties"]["after_sequence"]["default"], 0);
        assert_eq!(events["properties"]["limit"]["default"], 64);
        assert_eq!(events["properties"]["limit"]["maximum"], 256);
    }
}
