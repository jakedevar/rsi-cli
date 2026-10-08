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
mod manager_v2;

/// Version of the deterministic schema-discovery envelope.
pub const AGENT_CONTROL_SCHEMA_VERSION_V1: u32 = 1;

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
    TopologyExecute,
    TopologyGetExecution,
    TopologyInterrupt,
    TopologyResolveAttempt,
    EnqueueLandingSource,
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
    QueryFailureSignatures,
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
    RsiControlTopologyExecute,
    RsiControlTopologyGetExecution,
    RsiControlTopologyInterrupt,
    RsiControlTopologyResolveAttempt,
    RsiControlReadSessionEvents,
    RsiControlQueryFailureSignatures,
    RsiControlGlobalOverview,
    RsiControlGlobalSend,
    RsiControlReportToGlobal,
    RsiControlReportUp,
    RsiControlSendDown,
    RsiControlManagerOverview,
    RsiControlCreateProject,
    RsiControlUpdateProject,
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
            Self::RsiControlTopologyExecute => "rsi_control_topology_execute",
            Self::RsiControlTopologyGetExecution => "rsi_control_topology_get_execution",
            Self::RsiControlTopologyInterrupt => "rsi_control_topology_interrupt",
            Self::RsiControlTopologyResolveAttempt => "rsi_control_topology_resolve_attempt",
            Self::RsiControlReadSessionEvents => "rsi_control_read_session_events",
            Self::RsiControlQueryFailureSignatures => "rsi_control_query_failure_signatures",
            Self::RsiControlGlobalOverview => "rsi_control_global_overview",
            Self::RsiControlGlobalSend => "rsi_control_global_send",
            Self::RsiControlReportToGlobal => "rsi_control_report_to_global",
            Self::RsiControlReportUp => "rsi_control_report_up",
            Self::RsiControlSendDown => "rsi_control_send_down",
            Self::RsiControlManagerOverview => "rsi_control_manager_overview",
            Self::RsiControlCreateProject => "rsi_control_create_project",
            Self::RsiControlUpdateProject => "rsi_control_update_project",
        }
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
            Self::RsiControlTopologyExecute => AgentControlVerbV1::TopologyExecute,
            Self::RsiControlTopologyGetExecution => AgentControlVerbV1::TopologyGetExecution,
            Self::RsiControlTopologyInterrupt => AgentControlVerbV1::TopologyInterrupt,
            Self::RsiControlTopologyResolveAttempt => AgentControlVerbV1::TopologyResolveAttempt,
            Self::RsiControlReadSessionEvents => AgentControlVerbV1::ReadSessionEvents,
            Self::RsiControlQueryFailureSignatures => AgentControlVerbV1::QueryFailureSignatures,
            Self::RsiControlGlobalOverview => AgentControlVerbV1::GlobalOverview,
            Self::RsiControlGlobalSend => AgentControlVerbV1::GlobalSend,
            Self::RsiControlReportToGlobal => AgentControlVerbV1::ReportToGlobal,
            Self::RsiControlReportUp => AgentControlVerbV1::ReportUp,
            Self::RsiControlSendDown => AgentControlVerbV1::SendDown,
            Self::RsiControlManagerOverview => AgentControlVerbV1::ManagerOverview,
            Self::RsiControlCreateProject => AgentControlVerbV1::CreateProject,
            Self::RsiControlUpdateProject => AgentControlVerbV1::UpdateProject,
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

    /// Deterministic, compact v1 discovery envelope.
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
                // S1.b: topology, deploy and landing.
                | Self::TopologyUpsert
                | Self::TopologyList
                | Self::TopologyExecute
                | Self::TopologyGetExecution
                | Self::TopologyInterrupt
                | Self::RequestDeploy
                | Self::EnqueueLandingSource
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
                |r: &crate::harness_manager_v2::AgentSubmitReviewReceiptRequestV1| r.validate()
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
            Self::ManagerLaunchIssueWorker => decode!(
                crate::manager_issue_worker::AgentManagerLaunchIssueWorkerRequestV1,
                |r: &crate::manager_issue_worker::AgentManagerLaunchIssueWorkerRequestV1| r
                    .validate()
            ),
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
            Self::ReadSessionEvents => decode!(
                crate::agent_session_events::AgentReadSessionEventsRequestV1,
                |r: &crate::agent_session_events::AgentReadSessionEventsRequestV1| r.validate()
            ),
            Self::GetProviderStatus => decode!(
                crate::agent_provider_status::AgentGetProviderStatusRequestV1,
                |r: &crate::agent_provider_status::AgentGetProviderStatusRequestV1| r.validate()
            ),
            Self::SubmitJob => decode!(
                crate::agent_jobs::AgentSubmitJobRequestV1,
                |r: &crate::agent_jobs::AgentSubmitJobRequestV1| r.typed_params().map(drop)
            ),
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
            Self::RequestDeploy => decode!(
                crate::agent_deploy::AgentRequestDeployRequestV1,
                |r: &crate::agent_deploy::AgentRequestDeployRequestV1| r.validate()
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
        }
    }
}

/// Stable, redacted local validation result. `field` is intentionally drawn
/// from a fixed allowlist and never includes serde diagnostics or input data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentControlParamErrorV1 {
    pub class: &'static str,
    pub field: &'static str,
}

impl AgentControlParamErrorV1 {
    const fn params() -> Self {
        Self {
            class: "invalid_input",
            field: "params",
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

/// Supported params for the caller-bound `AgentListWakes` surface. The owner is
/// the authenticated caller's session only; no field can name another session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListWakesParams {
    /// Also return disabled (cancelled, fired or suspended) jobs. Default false.
    #[serde(default)]
    pub include_disabled: bool,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl AgentListWakesParams {
    #[must_use]
    pub fn limit_is_valid(&self) -> bool {
        self.limit
            .is_none_or(|limit| (1..=LIST_WAKES_MAX_LIMIT).contains(&limit))
    }
}

const SPAWN_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["kind","query","idempotency_key"],"properties":{"kind":{"type":"string","enum":["Story","Task","Bug","Feature","Refactor","Research"],"description":"Child session kind"},"provider":{"type":["string","null"],"enum":["Claude","Codex","Pioneer","OpenRouter","Bedrock","Local","Antigravity","CodexAppServer","Harness","Gemini",null],"default":null,"description":"Optional child provider; omit to inherit the caller provider"},"model":{"type":["string","null"],"default":null,"description":"Optional model override for the child"},"effort":{"type":["string","null"],"default":null,"description":"Optional reasoning-effort hint"},"agent_role":{"type":["string","null"],"minLength":1,"maxLength":64,"default":null,"description":"Optional normalized display role; caller and Epic authority remain transport-bound"},"query":{"type":"string","minLength":1,"maxLength":262144,"description":"The child's initial prompt or task"},"topology_node":{"type":["string","null"],"default":null,"description":"Optional bound topology node id"},"iteration":{"type":["integer","null"],"minimum":0,"maximum":4294967295,"default":null,"description":"Optional iteration override; omit to auto-increment"},"tags":{"type":["array","null"],"maxItems":64,"items":{"type":"string"},"default":null,"description":"Optional tag override set"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Required stable dedup key; exact retries return the same request and child IDs"}}}"#;
const RESERVE_SUCCESSOR_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["kind","query","idempotency_key"],"properties":{"kind":{"type":"string","enum":["Story","Task","Bug","Feature","Refactor","Research"],"description":"Successor session kind"},"model":{"type":["string","null"],"default":null,"description":"Optional model override for the successor"},"effort":{"type":["string","null"],"default":null,"description":"Optional reasoning-effort hint"},"query":{"type":"string","minLength":1,"maxLength":262144,"description":"The successor's initial prompt or task"},"topology_node":{"type":["string","null"],"default":null,"description":"Optional bound topology node id"},"iteration":{"type":["integer","null"],"minimum":0,"maximum":4294967295,"default":null,"description":"Optional iteration override; omit to auto-increment"},"tags":{"type":["array","null"],"maxItems":64,"items":{"type":"string"},"default":null,"description":"Optional tag override set"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Required stable dedup key; exact retries return the same reservation and successor IDs"}}}"#;
const PROGRESS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"session_ids":{"type":"array","maxItems":256,"items":{"type":"string","format":"uuid"},"default":[],"description":"Optional authorized child subdivision; omit for the full cohort"}}}"#;
const SEND_MESSAGE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["target_session_id","message","idempotency_key"],"properties":{"target_session_id":{"type":"string","format":"uuid","description":"The child session to queue mail for"},"message":{"type":"string","minLength":1,"maxLength":16384,"description":"Message body queued for the target agent; acceptance does not prove delivery"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Required stable dedup key; exact retries return the original receipt and deadline"},"expires_at":{"type":["string","null"],"format":"date-time","default":null,"description":"Optional RFC3339 deadline; omit or send null for 30 minutes after first acceptance. An explicit deadline is preserved"}}}"#;
const READ_SESSION_EVENTS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["session_id"],"properties":{"session_id":{"type":"string","format":"uuid","description":"A session you may observe: your own child, a child of an Epic you lead, or a session in your manager scope"},"after_sequence":{"type":["integer","null"],"minimum":0,"default":null,"description":"Forward page: events with a greater sequence, ascending. Omit for the tail (newest events, ascending)."},"limit":{"type":["integer","null"],"minimum":1,"maximum":100,"default":20},"event_types":{"type":["array","null"],"maxItems":8,"items":{"type":"string","enum":["Message","ToolUse","ToolResult","System","Thinking","Compressed","CompletionGate"]},"default":null,"description":"Only these event types"},"max_bytes":{"type":["integer","null"],"minimum":1024,"maximum":262144,"default":32768,"description":"Page byte budget; content, tool_input and metadata are also clipped per event"},"final_message_full":{"type":"boolean","default":false,"description":"Return final_message.content up to 32768 characters instead of the default 8000-character cap"}}}"#;
const REQUEST_DEPLOY_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["sha","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"sha":{"type":"string","minLength":40,"maxLength":40,"pattern":"^[0-9a-f]{40}$","description":"Full lowercase commit SHA the binaries were built from"},"binaries_dir":{"type":["string","null"],"default":null,"description":"Absolute directory of freshly built binaries (rsid required); under the sandbox base, ~/.rsi/staging or ~/.cargo/shared-target"},"build":{"type":["boolean","null"],"default":null,"description":"Reserved: build at sha through the job path; refused deploy_build_not_supported"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Required stable dedup key; an exact retry returns the same deploy"},"max_wait_secs":{"type":["integer","null"],"minimum":1,"maximum":3600,"default":null,"description":"Bound on the wait for a quiet point; default 900"},"peer_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Deploy on this paired satellite over the hub link instead of on this daemon; binaries_dir is then a path on the satellite. Confirm with AgentGetDaemonInfo satellites"},"cancel":{"type":["boolean","null"],"default":null,"description":"true cancels your own waiting deploy named by idempotency_key and sha (binaries_dir not needed): it settles failed with deploy_cancelled and its hold on new launches ends at once; deploy_cancel_too_late once the restart began; not with peer_id"},"interrupt_workers":{"type":["boolean","null"],"default":null,"description":"true: once the operator's drain hold (deploy_drain_hold_secs) is over, a worker still mid-turn no longer blocks the quiet point; the restart interrupts it and the existing post-restart path resumes its turn with a continue prompt. A landing and a local test/build/landing job still block. The outcome lists the interrupted worker session ids and each is an andon friction event (deploy_interrupt). Default false; not with peer_id or cancel"}}}"#;
const QUERY_FAILURE_SIGNATURES_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"test_id":{"type":["string","null"],"minLength":1,"maxLength":512,"default":null,"description":"Exact libtest/nextest test id, e.g. session::launch::tests::x"},"digest":{"type":["string","null"],"pattern":"^[0-9a-f]{64}$","default":null,"description":"sha256 failure digest (rsi-known-failure block prints it); both fields must match when both are set"}}}"#;
const MANAGER_OVERVIEW_SCHEMA: &str =
    r#"{"type":"object","additionalProperties":false,"properties":{}}"#;
const CREATE_PROJECT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["name","path"],"properties":{"name":{"type":"string","minLength":1,"maxLength":128,"description":"Unique project name shown in the TUI tab bar"},"path":{"type":"string","minLength":1,"maxLength":4096,"description":"Absolute directory of the repository or working tree; it must exist and sit inside a configured workspace root; with no workspace roots configured, a strict descendant of the daemon user's home directory (not the home directory itself, anything under ~/.rsi, or /)"},"description":{"type":["string","null"],"maxLength":2048,"default":null},"color":{"type":["string","null"],"pattern":"^#[0-9a-fA-F]{6}$","default":null,"description":"Tab color as #rrggbb"}}}"#;
const UPDATE_PROJECT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["project_id"],"properties":{"project_id":{"type":"string","format":"uuid","description":"A project in your coverage: your own project (project manager) or a project of your grant (portfolio seat)"},"name":{"type":["string","null"],"minLength":1,"maxLength":128,"default":null},"path":{"type":["string","null"],"minLength":1,"maxLength":4096,"default":null,"description":"New absolute directory; refused while the project has a live session"},"description":{"type":["string","null"],"maxLength":2048,"default":null},"color":{"type":["string","null"],"pattern":"^#[0-9a-fA-F]{6}$","default":null}}}"#;
const GLOBAL_OVERVIEW_SCHEMA: &str =
    r#"{"type":"object","additionalProperties":false,"properties":{}}"#;
const MANAGER_LAUNCH_ISSUE_WORKER_BASE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["issue","brief","launch","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"issue":{"type":"integer","minimum":1,"description":"The Issue's display number in your project"},"brief":{"type":"string","minLength":1,"maxLength":24576,"description":"The worker's task text; it reads its own Issue with AgentGetIssue, so do not paste the Issue"},"launch":{"type":"object","additionalProperties":false,"required":["provider","model"],"properties":{"provider":{"type":"string","description":"Provider; must be in your policy's allowed_launches"},"model":{"type":"string","minLength":1,"maxLength":256},"effort":{"type":["string","null"],"maxLength":32,"default":null}}},"parent_epic_id":{"type":["string","null"],"format":"uuid","default":null,"description":"The Epic to create the worker under; optional when your scope holds exactly one Epic"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key; a replay returns the same worker"}}}"#;
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
        "description": "#1254: the terminal worker of this Issue to continue after it passed the baton; the new worker branches from its committed HEAD, inherits its lineage and starts from its final message and the Issue's latest handoff. Exclusive with sandbox_source."
    });
    schema["properties"]["review_of"] = serde_json::json!({
        "type": ["integer", "null"],
        "minimum": 1,
        "default": null,
        "description": "#1590: display number of the implementer Issue this worker reviews. The daemon copies that Issue's text and its complete latest handoff (landing filters included) into the worker's brief; a bound reviewer cannot read it. Do not paste the handoff yourself."
    });
    schema["properties"]["qa_lane"] = serde_json::json!({
        "type": "boolean", "default": false,
        "description": "Explicit manager delegation of bounded QA shard jobs (5-90 minutes, two running per owner); default false, including continuation. Not inherited."
    });
    schema.to_string()
});
const GLOBAL_SEND_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["project_id","message","idempotency_key"],"properties":{"project_id":{"type":"string","format":"uuid","description":"A project in your grant; the message goes to its current project manager"},"message":{"type":"string","minLength":1,"maxLength":32768,"description":"The full request or note for that project manager, with acceptance criteria"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key; replay only identical content under it"}}}"#;
const GLOBAL_APPOINT_MANAGER_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["project_id","launch","query","idempotency_key"],"properties":{"project_id":{"type":"string","format":"uuid","description":"A project in your grant"},"launch":{"type":"object","additionalProperties":false,"required":["provider","model"],"properties":{"provider":{"type":"string","enum":["Claude","Codex","Pioneer","OpenRouter","Bedrock","Local","Antigravity","CodexAppServer","Harness","Gemini"],"description":"Provider; must be in your grant's allowed_launches"},"model":{"type":"string","minLength":1,"maxLength":128,"description":"Model; must be in your grant's allowed_launches"},"effort":{"type":["string","null"],"default":null,"description":"Effort; must match the allowlist entry when it names one"}},"description":"The new project manager's launch"},"query":{"type":"string","minLength":1,"maxLength":65536,"description":"The new project manager's self-contained first prompt"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, e.g. gm-<project>-<purpose>-<n>; a replay returns the same session"},"sandbox":{"type":["boolean","null"],"default":null,"description":"Launch in its own git worktree sandbox (default true)"}}}"#;
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
                            "node_id": uuid("null creates a child node; a child node you granted replaces its seat (then send no other target field but launch_project_id and expected_grant_version)"),
                            "tier_label": {"type": ["string", "null"], "minLength": 1, "maxLength": 32, "default": null, "description": "Display label of a new child (authority never reads it)"},
                            "project_ids": {"type": "array", "items": {"type": "string", "format": "uuid"}, "maxItems": 64, "default": [], "description": "A strict subset of your coverage, disjoint from your other children"},
                            "allowed_launches": {"type": "array", "items": launch.clone(), "maxItems": 16, "default": [], "description": "Launches the child may make (default: yours); a subset of yours"},
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
            "sandbox": {"type": ["boolean", "null"], "default": null, "description": "Launch in its own git worktree sandbox (default true)"}
        }
    });
    schema.to_string()
});
const MANAGER_REVOKE_CHILD_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["node_id","expected_grant_version","idempotency_key"],"properties":{"node_id":{"type":"string","format":"uuid","description":"A child node your node granted"},"expected_grant_version":{"type":"integer","minimum":1,"description":"The child's current grant version (CAS)"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key; a replay against the revoked child returns it"}}}"#;
const REPORT_TO_GLOBAL_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["message","idempotency_key"],"properties":{"message":{"type":"string","minLength":1,"maxLength":32768,"description":"Your report to the global manager: landed work, a gate that blocks you, or your handoff path"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key; replay only identical content under it"}}}"#;
const REPORT_UP_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["message","idempotency_key"],"properties":{"message":{"type":"string","minLength":1,"maxLength":32768,"description":"Your report to the manager one level above you: landed work, a gate that blocks you, or your handoff path"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key; replay only identical content under it"}}}"#;
const SEND_DOWN_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["target","message","idempotency_key"],"properties":{"target":{"description":"The descendant manager node to message (a node below yours, inside your coverage)","oneOf":[{"type":"object","additionalProperties":false,"required":["kind","node_id"],"properties":{"kind":{"const":"portfolio"},"node_id":{"type":"string","format":"uuid"}}},{"type":"object","additionalProperties":false,"required":["kind","project_id"],"properties":{"kind":{"const":"project"},"project_id":{"type":"string","format":"uuid"}}},{"type":"object","additionalProperties":false,"required":["kind","node_id"],"properties":{"kind":{"const":"area"},"node_id":{"type":"string","format":"uuid"}}}]},"message":{"type":"string","minLength":1,"maxLength":32768,"description":"The full request or note for that manager, with acceptance criteria"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key; replay only identical content under it"}}}"#;
const GET_DAEMON_INFO_SCHEMA: &str =
    r#"{"type":"object","additionalProperties":false,"properties":{}}"#;
const GET_PROVIDER_STATUS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"provider":{"type":["string","null"],"enum":["claude","codex","pioneer","openrouter","bedrock","local","antigravity","codex_app_server","harness","anthropic","openai",null],"default":null,"description":"Provider to report; omit for every provider"}}}"#;
const TARGET_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Target session UUID; omit to target this session"}}}"#;
const WAKE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["message","mode"],"properties":{"message":{"type":"string","description":"Prompt to run when the wake fires"},"in_seconds":{"type":["integer","null"],"minimum":1,"default":null,"description":"Fire this many seconds from now; mutually exclusive with at"},"at":{"type":["string","null"],"format":"date-time","default":null,"description":"RFC3339 absolute fire time; mutually exclusive with in_seconds"},"name":{"type":["string","null"],"default":null,"description":"Optional human-readable job name"},"every_seconds":{"type":["integer","null"],"minimum":1,"default":null,"description":"Optional recurring interval in seconds"},"mode":{"type":"string","enum":["fresh","resume","on_terminal","program_guard","when"],"description":"Required: fresh is a consumed, best-effort root launch after this session is terminal and transfers no hierarchy or lead authority; use AgentReserveSuccessor for master turnover. resume re-invokes this session with context; on_terminal arms a terminal watch; program_guard registers daemon-authoritative program identity; when arms ONE resume wake that the daemon fires when the `when` predicate is true (no model turn is spent polling) and carries each job's id, name, state, exit code and refusal"},"watch_session_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Watched subject UUID; requires mode on_terminal and cannot steer the caller-bound wake target"},"when":{"type":["object","null"],"default":null,"additionalProperties":false,"description":"Required with mode when (and only then): exactly one predicate the daemon evaluates. jobs_terminal fires once every listed AgentSubmitJob job you own is terminal (submit them with wake none for one wake per batch); sha_on_rolling fires once that commit is an ancestor of origin/rolling in your repository (polled). in_seconds, at and every_seconds are not accepted with mode when","properties":{"jobs_terminal":{"type":"array","items":{"type":"string","format":"uuid"},"minItems":1,"maxItems":32,"description":"Job ids you own; an unknown or foreign id is refused at scheduling time"},"sha_on_rolling":{"type":"string","pattern":"^[0-9a-f]{40}$","description":"Full lowercase 40-hex commit id"}}},"timeout_seconds":{"type":["integer","null"],"minimum":1,"maximum":604800,"default":null,"description":"mode when only: fire with timed_out true after this many seconds if the predicate is still pending, so you are never stranded"}}}"#;
const CANCEL_WAKE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"job_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Job id returned by AgentScheduleWake; must be one of your own jobs. Provide exactly one of job_id or name"},"name":{"type":["string","null"],"minLength":1,"maxLength":256,"default":null,"description":"Name of your own scheduled job(s) to disable. Provide exactly one of job_id or name"}}}"#;
const LIST_WAKES_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"include_disabled":{"type":"boolean","default":false,"description":"Also return disabled (cancelled, fired or suspended) jobs"},"limit":{"type":["integer","null"],"minimum":1,"maximum":256,"default":64,"description":"Maximum jobs returned, soonest fire first"}}}"#;
const CREATE_ISSUE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["title","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"title":{"type":"string","minLength":1,"maxLength":512},"body":{"type":"string","maxLength":65536,"default":""},"priority":{"type":["integer","null"],"minimum":1,"maximum":4,"default":null},"labels":{"type":"array","maxItems":64,"items":{"type":"string","maxLength":128},"default":[]},"assignee":{"type":["string","null"],"maxLength":256,"default":null},"idempotency_key":{"type":"string","minLength":1,"maxLength":128},"harness":{"type":"boolean","default":false,"description":"File in the RSI harness project instead of your own: use for a defect in RSI itself found while working in another project. Cannot combine with project_id"},"source_issue":{"type":["integer","null"],"minimum":1,"default":null,"description":"With harness: display number of the Issue in your own project this one mirrors"}}}"#;
const LIST_ISSUES_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"status":{"type":["string","null"],"enum":["Open","InProgress","Closed","Cancelled",null],"default":null},"archive":{"type":"string","enum":["Active","Archived","All"],"default":"Active"},"cursor":{"type":["object","null"],"additionalProperties":false,"required":["display_number","issue_id"],"properties":{"display_number":{"type":"integer","minimum":1},"issue_id":{"type":"string","format":"uuid"}},"default":null},"limit":{"type":["integer","null"],"minimum":1,"maximum":256,"default":64},"ready":{"type":"boolean","default":false,"description":"When true, include only open, active Issues with no open or in-progress blockers"},"order":{"type":"string","enum":["asc","desc"],"default":"asc","description":"Sort by display number; desc lists newest first. The cursor works in both orders"},"title_contains":{"type":["string","null"],"minLength":1,"maxLength":128,"default":null,"description":"Case-insensitive substring filter over Issue titles"}}}"#;
const GET_ISSUE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"issue_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Issue UUID; set exactly one of issue_id and display_number"},"display_number":{"type":["integer","null"],"minimum":1,"default":null,"description":"Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"}}}"#;
const UPDATE_ISSUE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["expected_row_version","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"issue_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Issue UUID; set exactly one of issue_id and display_number"},"display_number":{"type":["integer","null"],"minimum":1,"default":null,"description":"Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128},"title":{"type":["string","null"],"maxLength":512,"default":null},"body":{"type":["string","null"],"maxLength":65536,"default":null},"labels":{"type":["array","null"],"maxItems":64,"items":{"type":"string","maxLength":128},"default":null},"priority":{"type":["integer","null"],"minimum":1,"maximum":4,"default":null},"clear_priority":{"type":"boolean","default":false},"assignee":{"type":["string","null"],"maxLength":256,"default":null},"clear_assignee":{"type":"boolean","default":false}}}"#;
const UPDATE_ISSUE_STATUS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["status","expected_row_version","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"issue_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Issue UUID; set exactly one of issue_id and display_number"},"display_number":{"type":["integer","null"],"minimum":1,"default":null,"description":"Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"},"status":{"type":"string","enum":["Open","InProgress","Closed","Cancelled"]},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const ISSUE_CAS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["issue_id","expected_row_version","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"issue_id":{"type":"string","format":"uuid"},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const LIST_ISSUE_EVENTS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["issue_id"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"issue_id":{"type":"string","format":"uuid"},"after_sequence":{"type":"integer","minimum":0,"default":0},"limit":{"type":["integer","null"],"minimum":1,"maximum":256,"default":64}}}"#;

const CONTINUE_CHILD_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["target_session_id","query","expected_tip_session_id","expected_event_sequence"],"properties":{"target_session_id":{"type":"string","format":"uuid","description":"The child session to continue; the caller itself is refused"},"query":{"type":"string","minLength":1,"maxLength":262144,"description":"Continuation prompt delivered as the child's next turn"},"expected_tip_session_id":{"type":"string","format":"uuid","description":"Required staleness fence: the lineage tip the caller last observed"},"expected_event_sequence":{"type":"integer","minimum":0,"description":"Required staleness fence: MAX(sequence) of the tip's conversation events as last observed"},"expected_custody_generation":{"type":["integer","null"],"minimum":1,"default":null,"description":"Optional staleness fence: the sandbox custody generation as last observed; must match exactly including absence"}}}"#;
const ARCHIVE_CHILD_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["target_session_id","expected_tip_session_id","expected_event_sequence"],"properties":{"target_session_id":{"type":"string","format":"uuid","description":"Terminal child to archive; current Epic lead only"},"expected_tip_session_id":{"type":"string","format":"uuid","description":"Required staleness fence: last observed lineage tip"},"expected_event_sequence":{"type":"integer","minimum":0,"description":"Required staleness fence: last observed tip event sequence"},"expected_custody_generation":{"type":["integer","null"],"minimum":1,"default":null,"description":"Optional staleness fence: last observed sandbox custody generation"}}}"#;

const MANAGER_PROGRESS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"after_epic_id":{"type":["string","null"],"format":"uuid","description":"Continue after next_after_epic_id from the previous page."},"limit":{"type":["integer","null"],"minimum":1,"maximum":64,"default":32}}}"#;
const MANAGER_INBOX_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"after_notice_sequence":{"type":"integer","minimum":0,"default":0},"notice_kind":{"type":["string","null"],"enum":["session_state","message","action_result","operator_answer","ledger_change",null],"default":null},"settle_notice_ids":{"type":"array","items":{"type":"string","format":"uuid"},"maxItems":32,"default":[]},"after_sequence":{"type":"integer","minimum":0,"default":0},"limit":{"type":"integer","minimum":1,"maximum":32,"default":32},"request_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Optional recorded request to read within your live manager or feature-lead scope"}}}"#;
const MANAGER_SEND_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["epic_id","message","idempotency_key"],"properties":{"epic_id":{"type":"string","format":"uuid","description":"An Epic in your operator-appointed manager scope; the daemon resolves its current lead"},"message":{"type":"string","minLength":1,"maxLength":8192,"description":"Nonblank request, at most 8192 UTF-8 bytes, without NUL"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"},"informational":{"type":"boolean","default":false,"description":"Deliver auditable information without reserving a pending request slot or requiring a reply"}}}"#;
const MANAGER_REPLY_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["request_id","message","idempotency_key"],"properties":{"request_id":{"type":"string","format":"uuid","description":"Recorded request addressed to the Epic you currently lead"},"message":{"type":"string","minLength":1,"maxLength":8192,"description":"Explicit reply with evidence or a blocker, at most 8192 UTF-8 bytes, without NUL"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"},"still_running":{"type":"boolean","default":false,"description":"Keep the request active after this reply; omit or false to settle it"}}}"#;
const MANAGER_NOTIFY_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["message","idempotency_key"],"properties":{"message":{"type":"string","minLength":1,"maxLength":8192,"description":"Nonblank informational notice to your current appointed manager, at most 8192 UTF-8 bytes, without NUL; not a request, approval or acceptance"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"}}}"#;

const MANAGER_WORK_VIEW_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"work_key":{"type":["string","null"],"minLength":1,"maxLength":256,"default":null,"description":"Optional exact work key in your Epic"},"after_work_key":{"type":["string","null"],"minLength":1,"maxLength":256,"default":null,"description":"Continue after next_after_work_key from the previous page"},"limit":{"type":"integer","minimum":1,"maximum":32,"default":32}}}"#;
const MANAGER_DELEGATE_NODE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["expected_node_grant_version","expected_parent_authority_epoch","expected_parent_grant_version","expected_parent_policy_version","grant","idempotency_key","node_id","parent_node_id","policy","seat_root_session_id","selector"],"properties":{"node_id":{"type":["string","null"],"format":"uuid"},"parent_node_id":{"type":"string","format":"uuid"},"seat_root_session_id":{"type":"string","format":"uuid"},"selector":{"type":"object"},"grant":{"type":"object"},"policy":{"type":"object"},"expected_parent_grant_version":{"type":"integer","minimum":1},"expected_parent_policy_version":{"type":"integer","minimum":0},"expected_parent_authority_epoch":{"type":"integer","minimum":1},"expected_node_grant_version":{"type":"integer","minimum":0},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const MANAGER_ESCALATE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["expected_source_authority_epoch","expected_source_grant_version","expected_target_authority_epoch","expected_target_grant_version","expected_target_session_id","idempotency_key","reason","route","subject_id"],"properties":{"subject_id":{"type":"string","format":"uuid"},"reason":{"type":"string","minLength":1,"maxLength":8192},"route":{"type":"object"},"expected_source_authority_epoch":{"type":"integer","minimum":1},"expected_source_grant_version":{"type":"integer","minimum":1},"expected_target_authority_epoch":{"type":"integer","minimum":1},"expected_target_grant_version":{"type":"integer","minimum":1},"expected_target_session_id":{"type":"string","format":"uuid"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const MANAGER_LIST_ESCALATIONS_SCHEMA: &str =
    r#"{"type":"object","additionalProperties":false,"properties":{}}"#;
const MANAGER_RESOLVE_ESCALATION_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["escalation_id","expected_target_authority_epoch","expected_target_grant_version","expected_target_session_id","expected_version","idempotency_key","ruling"],"properties":{"escalation_id":{"type":"string","format":"uuid"},"expected_version":{"type":"integer","minimum":1},"expected_target_authority_epoch":{"type":"integer","minimum":1},"expected_target_grant_version":{"type":"integer","minimum":1},"expected_target_session_id":{"type":"string","format":"uuid"},"ruling":{"type":["string","null"],"maxLength":8192},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;

const GET_AUTHORITY_CATALOG_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"verb":{"type":["string","null"],"default":null,"description":"Optional control name: an Agent* method, a native rsi_control_* tool, or its mcp__rsi-agent__ spelling. With verb the response is compact: just that control's detail (whether you may call it, parameter schema, one minimal valid example request, and its stable refusal codes with next steps); omit verb for the full manual."}}}"#;

const SUBMIT_JOB_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["kind","params"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"sandbox_session_id":{"type":["string","null"],"format":"uuid","default":null,"description":"landing and cloud_gate only: run in this terminal, in-reach session's sandbox"},"kind":{"type":"string","enum":["test","build","landing","cloud_gate","cloud_sweep"]},"params":{"type":"object","description":"test: {recipe: declared name in the worktree .rsi/jobs.toml (version 1; recipes.NAME: runner just|make, target, timeout_minutes cap 5-180, cpu_quota_percent 1-1600); uses local justfile/Makefile; no arguments; request timeout must fit the declaration} or {scoped_test:{base,head?}: run scripts/scoped-test --base <ref> [--head <ref>] in your sandbox (bare branch, origin/<branch> or 40-hex refs only, no other arguments; submit with wake none, arm ONE AgentScheduleWake mode when, end your turn; result.exit_code and the job log_path are set, result.receipt is the scoped-test receipt {ok,exit_code,base,head,filters,log_dir,packages[{package,status,completed}]}, refusal scoped_test_receipt_missing when none was printed)} or {shard,filterset?} or {package,filters[],lib_only?,exact?:boolean (default false; true passes libtest --exact for whole test names; package only; empty filters still run every test)} or {candidate_receipt: branch or 40-hex sha; manager/Epic lead only; result.receipt is the typed candidate receipt}; every test job also takes timeout_minutes? (5-180; default the operator job_test_timeout_mins, 20; above the default needs the manager or an Epic lead, except qa_lane:{sha:40-lowercase-hex} on a known shard: current live unsuperseded AgentManagerLaunchIssueWorker binding with explicit manager qa_lane:true delegation or manager/Epic lead, timeout 5-90, default capped at 90, SHA labels submission-time HEAD only; no dirtiness check or exact-tested-bytes proof; use a manager pinned detached worktree plus worktree on an ordinary test for an exact pin, at most two running QA lanes per owner; QA never supports recipe, scoped_test, package or candidate_receipt; past timeout the unit is stopped and the job fails job_timed_out); build: {command:check|build,package|workspace,all_targets?,release?}; landing/cloud_gate: {accepted:40-hex sha,test_filters?:[PACKAGE=FILTER]}; cloud_sweep: {sha:40-hex current rolling tip}. Typed fields only; no command line."},"name":{"type":"string","minLength":1,"maxLength":80},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Optional replay key"},"worktree":{"type":"string","description":"Appointed manager only: another worktree of your own repository to run in; default is your sandbox"},"wake":{"type":["string","null"],"enum":["owner","none",null],"default":"owner","description":"owner (default): one resume wake to you when this job settles. none: no per-job wake; the job still settles and AgentGetJob/AgentListJobs return its result. Submit a batch with none, then arm one AgentScheduleWake mode when with when.jobs_terminal so you are woken once"}}}"#;

const GET_JOB_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["job_id"],"properties":{"job_id":{"type":"string","format":"uuid"}}}"#;

const CANCEL_JOB_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["job_id"],"properties":{"job_id":{"type":"string","format":"uuid"}}}"#;

const LIST_JOBS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"limit":{"type":"integer","minimum":1,"maximum":100}}}"#;

const REPORT_TO_HUB_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["kind","text"],"properties":{"kind":{"type":"string","enum":["enqueue","deploy_ready","result"],"description":"Report line type (ENQUEUE, DEPLOY-READY, RESULT)"},"text":{"type":"string","minLength":1,"maxLength":1024,"description":"Short report line; informational and untrusted on the hub"}}}"#;
const SEND_SATELLITE_MESSAGE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["peer_id","remote_session_id","message","idempotency_key"],"properties":{"peer_id":{"type":"string","description":"Operator-registered satellite peer id"},"remote_session_id":{"type":"string","description":"Session id on the satellite, inside the operator-declared scope"},"message":{"type":"string","minLength":1,"maxLength":16384,"description":"Message text; queued until the remote session is idle"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"},"expires_at":{"type":"string","description":"Optional RFC3339 expiry, at most 24 hours ahead; default 30 minutes"}}}"#;
const ENQUEUE_LANDING_SOURCE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["source_commit","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"source_session_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Land from this in-reach session's sandbox instead of your own"},"source_commit":{"type":"string","pattern":"^[0-9a-f]{40}$","description":"Full lowercase 40-hex accepted source commit reachable from your sandbox"},"test_filters":{"type":"array","maxItems":32,"items":{"type":"string","minLength":3,"maxLength":256,"description":"PACKAGE=FILTER; the gate is a compile check plus the named tests"},"default":[]},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"}}}"#;

const TOPOLOGY_UPSERT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["name","definition","scope","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"name":{"type":"string","minLength":1,"maxLength":128,"description":"Topology name, unique per owner"},"definition":{"type":"object","required":["nodes","edges"],"properties":{"nodes":{"type":"array","items":{"type":"object"}},"edges":{"type":"array","items":{"type":"object"}},"until":{"type":["object","null"]}},"description":"TopologyDefinition: nodes (id, kind, label, prereqs, params incl. typed step, custody, explicit provider/model/effort), edges, until; the daemon validates it and returns diagnostics"},"scope":{"type":"string","enum":["epic","manager"],"description":"epic: owned by one Epic (leads may only use this); manager: owned by the current manager"},"epic_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Target Epic for a manager epic-scoped upsert; a lead's Epic is daemon-derived and, if given, must match"},"expected_revision":{"type":["integer","null"],"minimum":1,"default":null,"description":"CAS fence to revise an existing topology; omit to create"},"validate_only":{"type":"boolean","default":false,"description":"Report diagnostics without writing"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"}}}"#;
const TOPOLOGY_LIST_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"scope":{"type":["string","null"],"enum":["epic","manager",null],"default":null},"epic_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Narrow to one Epic in your scope"},"include_executions":{"type":"boolean","default":false},"cursor":{"type":["string","null"],"maxLength":128,"default":null,"description":"next_cursor from the previous page"},"limit":{"type":["integer","null"],"minimum":1,"maximum":32,"default":null}}}"#;
const TOPOLOGY_EXECUTE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["topology_id","expected_digest","epic_id","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"topology_id":{"type":"string","format":"uuid"},"expected_digest":{"type":"string","pattern":"^sha256:[0-9a-f]{64}$","description":"definition_digest returned by list or upsert"},"epic_id":{"type":"string","format":"uuid","description":"Epic the execution runs under; must be in your scope"},"inputs":{"type":["object","null"],"default":null},"on_call":{"description":"Seat that answers node questions: {\"kind\":\"project_manager\"} (default: the project manager, else the covering portfolio seat) or {\"kind\":\"portfolio\",\"node_id\":uuid}","default":null,"oneOf":[{"type":"null"},{"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"project_manager"}}},{"type":"object","additionalProperties":false,"required":["kind","node_id"],"properties":{"kind":{"const":"portfolio"},"node_id":{"type":"string","format":"uuid"}}}]},"base_commit":{"type":["string","null"],"pattern":"^[0-9a-fA-F]{40}$","default":null,"description":"Full 40-hex base; omitted resolves origin/rolling once at acceptance"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; a replay returns deduplicated=true"}}}"#;
const TOPOLOGY_GET_EXECUTION_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["execution_id"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"execution_id":{"type":"string","format":"uuid"},"after_sequence":{"type":["integer","null"],"minimum":0,"default":null,"description":"next_sequence from the previous page"},"limit":{"type":["integer","null"],"minimum":1,"maximum":64,"default":null}}}"#;
const TOPOLOGY_INTERRUPT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["execution_id","expected_row_version","idempotency_key"],"properties":{"project_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Global manager only: the granted project to act in; omit for your own project"},"execution_id":{"type":"string","format":"uuid"},"expected_row_version":{"type":"integer","minimum":1,"description":"row_version observed via get_execution; refresh after stale_version"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"}}}"#;
const TOPOLOGY_RESOLVE_ATTEMPT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["execution_id","attempt_id","action","expected_row_version","idempotency_key"],"properties":{"execution_id":{"type":"string","format":"uuid"},"attempt_id":{"type":"string","format":"uuid","description":"Attempt blocked on preserved work"},"action":{"type":"string","enum":["inspect","accept","retry","discard"],"description":"discard is manager (Automation) only; a lead is refused"},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"},"confirm_preserved_commit":{"type":["string","null"],"pattern":"^[0-9a-fA-F]{40}$","default":null,"description":"Full 40-hex preserved_commit; required iff action=discard, refused otherwise"}}}"#;

static AGENT_CONTROL_CATALOG_V1: LazyLock<[AgentControlDescriptorV1; 67]> = LazyLock::new(|| {
    [
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetAuthorityCatalog,
            method: "AgentGetAuthorityCatalog",
            description: "Your operator's manual: your current roles, the operating rules for those roles, and exactly the controls you may call now. Pass verb for one control only: its parameter schema, a minimal valid example and its refusal codes (no full list or guidance is repeated). Read-only and open to every session; call it first and again after a role change or an authority refusal.",
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
            parameters_json: TARGET_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlStatus),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::Halt,
            method: "AgentHalt",
            description: "Halt a running child or a scoped leaf with manager SessionControl Execute authority.",
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
            native_tool: None,
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
            description: "Schedule a future wake/callback; explicit mode is required: fresh, resume, on_terminal, program_guard, or when (use resume for same-session continuation; when is a daemon-evaluated predicate wake: jobs_terminal or sha_on_rolling, one resume wake, optional timeout_seconds). The current manager may watch subjects in its live scope.",
            parameters_json: WAKE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::ScheduleWake),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CancelWake,
            method: "AgentCancelWake",
            description: "Disable your own scheduled wake(s) by job_id or name so a stale safety net cannot block succession, pause or lead replacement; refuses other sessions' jobs and the daemon-owned program guard. Rows are disabled, not deleted. RPC-only.",
            parameters_json: CANCEL_WAKE_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListWakes,
            method: "AgentListWakes",
            description: "List your own scheduled wakes (job_id, name, mode, watch target, next_fire_at, enabled, created_at), bounded; caller-bound, so no other session's jobs are visible. RPC-only.",
            parameters_json: LIST_WAKES_SCHEMA,
            native_tool: None,
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
            description: "List a bounded page of Issues in the project owned by the Epic you currently lead, or by an appointed manager with issue-coordinate authority. Set ready=true to filter to the operator ready-work projection, order=desc for newest first, and title_contains for a case-insensitive title filter.",
            parameters_json: LIST_ISSUES_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlListIssues),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetIssue,
            method: "AgentGetIssue",
            description: "Read one Issue by issue_id or display_number (exactly one) with up to 256 blocked_by and blocks entries (each list has a *_truncated flag) in the project owned by the Epic you currently lead, or by an appointed manager with issue-coordinate authority.",
            parameters_json: GET_ISSUE_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlGetIssue),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::UpdateIssue,
            method: "AgentUpdateIssue",
            description: "CAS-update active Issue content (target by issue_id or display_number, exactly one) as the current owning-Epic lead or manager with issue-coordinate authority. A worker launched for one Issue (AgentManagerLaunchIssueWorker) may, while that binding is live, append to that Issue only: send just body, holding the current body unchanged as its prefix plus your note or handoff, with the row_version you read; any other field, Issue or status change is refused.",
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
            description: "Retrieve durable manager requests/replies; page notices with after_notice_sequence and notice_kind, or settle_notice_ids belonging to your seat. Returned notices settle automatically; retrieval does not reply, answer a decision, clear a question, grant approval, or prove provider acceptance.",
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
            description: "Read bounded scoped manager state and current policy fences; traversal completeness is not feature acceptance.",
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
            description: "Request a scoped lead, container, session or lead-assignment operation under an explicit grant and current fences; a queued receipt is not an effected action.",
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
            description: "Read one manager action receipt within the authenticated current manager's project and logical manager scope. A queued action that a waiting deploy holds also carries held {reason: deploy_draining, deploy_id, release_by}. A queued create_session (Issue-worker launches included) that the daemon holds while the host's 1-minute load is above the operator's host_load_admission_threshold carries held {reason: host_load, load, threshold, recent_admissions}: it stays queued, never refused, and starts on its own when the load drops, held creates released oldest-first across projects.",
            parameters_json: manager_v2::GET_ACTION.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerGetAction),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerLaunchIssueWorker,
            method: "AgentManagerLaunchIssueWorker",
            description: "Manager seat (#1100): launch one worker bound to one Issue in a single idempotent call. The daemon runs the create_session action (same authority, allowlist, capacity and Epic scope as AgentManagerControl; refused before any effect), appends an Issue note naming the worker, sets the Issue InProgress and arms your on_terminal watch on the worker once it exists. Needs the SessionCreate and IssueCoordinate grants. The bound worker may read only its own Issue with AgentGetIssue, so do not paste the Issue text into the brief; while it is live it may also append its handoff to that Issue's body with AgentUpdateIssue (no other field, status or Issue). A replay under the same idempotency_key returns the same worker. Poll the returned action with AgentManagerGetAction. The worker's sandbox branches from the freshly fetched origin/rolling tip by default; pass sandbox_source {\"path\": \"<your sandbox_root>\"} to build on your unlanded commits, or {\"commit\": \"<40-hex>\"} for an exact commit. For a review Issue pass review_of: <implementer Issue number>: the daemon copies that Issue's text and complete latest handoff into the reviewer's brief (a bound reviewer cannot read it; never paste a truncated copy). When a worker passed the baton (its context filled; you got a worker_context_cap notice) and has ended its turn, pass continue_from: <that worker's session id> to relaunch from its committed HEAD with its lineage and handoff in one call.",
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
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerEscalate,
            method: "AgentManagerEscalate",
            description: "Escalate one subject to the parent node or the nearest common ancestor of two Epic owners, with exact source and target fences.",
            parameters_json: MANAGER_ESCALATE_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerListEscalations,
            method: "AgentManagerListEscalations",
            description: "List escalations addressed to or sent by your live manager node; rulings stay on the logical node across seat succession.",
            parameters_json: MANAGER_LIST_ESCALATIONS_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ManagerResolveEscalation,
            method: "AgentManagerResolveEscalation",
            description: "Rule on an addressed escalation or forward it to your parent with exact custody and version fences; human approvals remain operator-owned.",
            parameters_json: MANAGER_RESOLVE_ESCALATION_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::TopologyUpsert,
            method: "AgentTopologyUpsert",
            description: "Create, revise or validate a scoped deterministic topology (current manager with Automation, or an Epic lead for its own Epic). Every session/review triple must be explicit and operator-allowed; returns the definition digest and diagnostics.",
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
            verb: AgentControlVerbV1::TopologyExecute,
            method: "AgentTopologyExecute",
            description: "Start one daemon-run execution of a visible topology on an Epic in your scope, fenced by its definition digest; policy is rechecked and created sessions are charged. Idempotent on the key.",
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
            description: "Enqueue one accepted source commit on the daemon-owned rolling merge queue (current appointed manager or current Epic lead). The queue gates and publishes it; the outcome wakes you once. Refused with queue_disabled while the operator has the queue off.",
            parameters_json: ENQUEUE_LANDING_SOURCE_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ReadSessionEvents,
            method: "AgentReadSessionEvents",
            description: "Read a bounded page of a session's conversation events (tail or after_sequence), with final_message and terminal_reason, for your own child, a child of an Epic you lead, or a session in your manager scope. Set final_message_full to return up to 32768 characters of the final message.",
            parameters_json: READ_SESSION_EVENTS_SCHEMA,
            native_tool: Some(NativeAgentControlToolV1::RsiControlReadSessionEvents),
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetProviderStatus,
            method: "AgentGetProviderStatus",
            description: "Read bounded, secret-free provider health for the current appointed manager or Epic lead: configured, reachable, remaining credit where the provider API exposes it (OpenRouter), last 402/429 time, recent launch failure rate, and whether a launch would be refused. The daemon calls provider APIs with its own credentials (cached 60 s) and never returns a key. RPC-only.",
            parameters_json: GET_PROVIDER_STATUS_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SubmitJob,
            method: "AgentSubmitJob",
            description: "Run a long test (including a declared just/make recipe), build, landing or cloud-gate operation as a daemon-owned durable job in your sandbox. Linux uses systemd; macOS uses launchd for package test/build and recipe jobs (shard, candidate-receipt, landing and cloud workflows, and other operating systems, are refused job_platform_unsupported). Package test params accept exact:true for whole test-name matching (libtest --exact; default false keeps substring filters). It survives your turn ending, your session and a daemon restart; one resume wake carries the typed result (wake none suppresses that per-job wake: submit a batch with wake none, then arm one AgentScheduleWake mode when with when.jobs_terminal for a single wake). The execution timeout (timeout_minutes, default job_test_timeout_mins) starts when the unit launches: a job held behind a deploy drain does not spend it, and settles failed with job_admission_timed_out if it is still unlaunched after the drain hold cap plus a margin. Dispatch it and end your turn.",
            parameters_json: SUBMIT_JOB_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetJob,
            method: "AgentGetJob",
            description: "Read one of your daemon-owned jobs: state, exit code, log path and typed result.",
            parameters_json: GET_JOB_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::ListJobs,
            method: "AgentListJobs",
            description: "List your daemon-owned jobs, newest first.",
            parameters_json: LIST_JOBS_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::CancelJob,
            method: "AgentCancelJob",
            description: "Stop one of your own running daemon-owned jobs (a mistaken or over-broad run) through its platform service label: the service is stopped and the job settles failed with refusal job_cancelled. Cancelling a job that already settled changes nothing.",
            parameters_json: CANCEL_JOB_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::SendSatelliteMessage,
            method: "AgentSendSatelliteMessage",
            description: "Queue one message for an idle session on a paired satellite host (current appointed manager only; the operator must enable dispatch and declare the target in scope). Delivery waits until the remote session is idle and never interrupts; `queued` is acceptance, not delivery. Every refusal is the same target_not_authorized.",
            parameters_json: SEND_SATELLITE_MESSAGE_SCHEMA,
            native_tool: None,
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
            description: "Read the running hub daemon's identity and health for the current appointed manager or Epic lead: embedded build SHA, sha256 of the running binary, start time, schema version, disk free for the data dir and sandbox base, 1/5/15 load average, and supervisor mode. `host_load` reports the host-load admission (threshold, supported, load, recent_admissions, holding and the held manager creates and topology nodes, oldest first). The appointed manager also gets `satellites`: each enabled paired satellite read over the link now (build_sha, binary_sha256, started_at, schema_version, supervisor_mode, last_deploy, or reachable:false). Secret-free: never returns an environment value. RPC-only.",
            parameters_json: GET_DAEMON_INFO_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::RequestDeploy,
            method: "AgentRequestDeploy",
            description: "Ask the daemon to deploy already-built binaries (current appointed manager holding the operator-granted Deploy capability, Execute mode, not paused). The daemon stages and verifies them, waits for a quiet point (no lander, job or scoped worker mid-turn; bounded by max_wait_secs), swaps them in, restarts under rsid-supervisor.sh with its environment intact and wakes you once with the outcome. A restart interrupts any worker still mid-turn (provider processes are not re-adopted; the interrupted turn is resumed), so the deploy waits for workers. While it waits it holds new worker starts, your own creates included, for at most the operator's deploy_drain_hold_secs (default 600); past that it waits for a lull without holding anything (AgentGetDaemonInfo deploy_drain shows waiting, release_by and blockers; AgentManagerGetAction shows a held action's reason). If a busy fleet never reaches a quiet point (a worker is always mid-turn), send the same request with interrupt_workers: true (keep max_wait_secs above the hold): past that hold a worker mid-turn no longer blocks, the restart interrupts it, the existing post-restart path resumes its turn with a continue prompt, the outcome wake lists the interrupted worker session ids and each is an andon friction event; a landing and a local test/build/landing job still block. To withdraw a waiting deploy, send its sha and idempotency_key with cancel: true and omit interrupt_workers. With peer_id it does the same on a paired satellite over the link (#1017; the operator must enable dispatch and declare the satellite's scope): the satellite runs its own deploy flow, nothing is stored on the hub, and you confirm through AgentGetDaemonInfo satellites (build_sha, last_deploy) and your own resume wake. RPC-only.",
            parameters_json: REQUEST_DEPLOY_SCHEMA,
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
            description: "Portfolio manager seat at any tier (#1239): in one idempotent call, launch a Standard root session (launch must be in your grant's allowed_launches) and seat it as a child: the project manager of a project in your coverage (displacing the current one as the operator's appoint does, saved with your child_policy), a new child manager node over a strict subset of your coverage (it narrows your grant in every dimension; sibling coverage stays disjoint; at most max_direct_reports children and project managers under you), or the replacement seat of a child node you granted (its grant, ledger and workers stay; the old seat loses authority at once). Every refusal happens before any session is created. Only the operator creates roots, adopts or re-parents. Returns {appointment_id, session_id, target_ref, scope_version, policy_version, deduplicated}; a replay returns the same session. AgentGlobalAppointManager is its project-target alias. RPC-only.",
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
            description: "Any manager seat (area, project or portfolio of any tier; #1240): one bounded read of your own manager node: its grant and seat, parent_of it, each child (child portfolio nodes, the project managers you manage directly, child areas) as a digest with its seat status, model, context fill %, grant version, coverage, summed Issue and session counts and pending escalations, the projects you manage directly (project manager seat and policy, Issue counts, Running and WaitingApproval counts, pending questions and approvals), the escalations waiting on you and a fleet rollup of your coverage (active agents, usage over 5m/1h/24h by project, provider and model). A child node's own projects and inbox are never included: send down to that child to ask. Refused for a caller that holds no manager node seat (manager_tier_not_node_seat). Read-only. AgentGlobalOverview is its v0-shaped alias.",
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
    ]
});

/// Iterate the only machine-readable method catalog exposed by `rsi-common`.
#[must_use]
pub fn agent_control_catalog_v1() -> &'static [AgentControlDescriptorV1] {
    &*AGENT_CONTROL_CATALOG_V1
}

#[cfg(test)]
mod tests {
    use super::*;
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
        AgentTopologyInterruptRequestV1, AgentTopologyListRequestV1, AgentTopologyUpsertRequestV1,
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
            AgentControlVerbV1::RequestDeploy => {
                serde_json::from_value::<crate::agent_deploy::AgentRequestDeployRequestV1>(value)
                    .map(drop)
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
                "AgentContinueChild",
                "AgentArchiveChild",
                "AgentCancelWake",
                "AgentListWakes",
                "AgentManagerDelegateNode",
                "AgentManagerEscalate",
                "AgentManagerListEscalations",
                "AgentManagerResolveEscalation",
                "AgentEnqueueLandingSource",
                "AgentGetProviderStatus",
                "AgentSubmitJob",
                "AgentGetJob",
                "AgentListJobs",
                "AgentCancelJob",
                "AgentSendSatelliteMessage",
                "AgentReportToHub",
                "AgentGetDaemonInfo",
                "AgentRequestDeploy",
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
            assert!(!first.contains('\n'));
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
        assert_eq!(
            AgentControlVerbV1::ListIssues
                .validate_params(&serde_json::json!({"ready":true,"extra":false})),
            Err(AgentControlParamErrorV1::params())
        );
        assert_eq!(
            AgentControlVerbV1::GetIssue.validate_params(&serde_json::json!({
                "issue_id":"5d73c05d-1040-49f7-92ab-0123456789ab","extra":false
            })),
            Err(AgentControlParamErrorV1::params())
        );

        let mut zero_fence = fixture(AgentControlVerbV1::ManagerControl);
        zero_fence["fence"]["scope_version"] = serde_json::json!(0);
        assert_eq!(
            AgentControlVerbV1::ManagerControl.validate_params(&zero_fence),
            Err(AgentControlParamErrorV1::params())
        );
        let oversized = serde_json::json!({
            "kind":"Task", "query":"x".repeat(262_145), "idempotency_key":"key"
        });
        assert_eq!(
            AgentControlVerbV1::SpawnChild.validate_params(&oversized),
            Err(AgentControlParamErrorV1::params())
        );
        let forged = serde_json::json!({"session_id":"5d73c05d-1040-49f7-92ab-0123456789ab","caller_session_id":"x"});
        assert_eq!(
            AgentControlVerbV1::GetStatus.validate_params(&forged),
            Err(AgentControlParamErrorV1::params())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fixed_schema_contract_fixtures_pin_all_verbs() {
        let shapes: &[(AgentControlVerbV1, &[&str], &[&str])] = &[
            (AgentControlVerbV1::GetAuthorityCatalog, &["verb"], &[]),
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
                    "target_session_id",
                ],
                &["target_session_id", "message", "idempotency_key"],
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
                    "target_session_id",
                ],
                &[
                    "target_session_id",
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
                    "target_session_id",
                ],
                &[
                    "target_session_id",
                    "expected_tip_session_id",
                    "expected_event_sequence",
                ],
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
                &["include_disabled", "limit"],
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
                    "idempotency_key",
                    "issue_id",
                    "labels",
                    "priority",
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
                    "issue_id",
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
                &["after_epic_id", "limit"],
                &[],
            ),
            (
                AgentControlVerbV1::ManagerInbox,
                &[
                    "after_notice_sequence",
                    "after_sequence",
                    "limit",
                    "notice_kind",
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
                &["cursor", "epic_id", "limit", "section"],
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
                &["assignment_id", "verdict", "findings", "idempotency_key"],
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
                &["cursor", "epic_id", "include_executions", "limit", "scope"],
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
                    "topology_id",
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
                    "wake",
                    "worktree",
                ],
                &["kind", "params"],
            ),
            (AgentControlVerbV1::GetJob, &["job_id"], &["job_id"]),
            (AgentControlVerbV1::ListJobs, &["limit"], &[]),
            (AgentControlVerbV1::CancelJob, &["job_id"], &["job_id"]),
            (
                AgentControlVerbV1::EnqueueLandingSource,
                &[
                    "idempotency_key",
                    "source_commit",
                    "source_session_id",
                    "test_filters",
                ],
                &["source_commit", "idempotency_key"],
            ),
            (AgentControlVerbV1::GetProviderStatus, &["provider"], &[]),
            (AgentControlVerbV1::GetDaemonInfo, &[], &[]),
            (
                AgentControlVerbV1::QueryFailureSignatures,
                &["digest", "test_id"],
                &[],
            ),
            (AgentControlVerbV1::GlobalOverview, &[], &[]),
            (AgentControlVerbV1::ManagerOverview, &[], &[]),
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
                &["idempotency_key", "launch", "query", "sandbox", "target"],
                &["target", "launch", "query", "idempotency_key"],
            ),
            (
                AgentControlVerbV1::ManagerRevokeChild,
                &["expected_grant_version", "idempotency_key", "node_id"],
                &["node_id", "expected_grant_version", "idempotency_key"],
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
                &["kind", "text"],
                &["kind", "text"],
            ),
            (
                AgentControlVerbV1::SendSatelliteMessage,
                &[
                    "expires_at",
                    "idempotency_key",
                    "message",
                    "peer_id",
                    "remote_session_id",
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
            (
                AgentControlVerbV1::ReadSessionEvents,
                &[
                    "after_sequence",
                    "event_types",
                    "final_message_full",
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
            progress["properties"]["session_ids"]["items"]["format"],
            "uuid"
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
            assert_eq!(target["properties"]["session_id"]["format"], "uuid");
        }

        let wake = AgentControlVerbV1::ScheduleWake.descriptor().parameters();
        assert_eq!(
            wake["properties"]["mode"]["enum"],
            serde_json::json!(["fresh", "resume", "on_terminal", "program_guard", "when"])
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
        assert_eq!(update["properties"]["labels"]["maxItems"], 64);
        assert_eq!(update["properties"]["priority"]["maximum"], 4);
        assert_eq!(update["properties"]["clear_priority"]["default"], false);
        assert_eq!(update["properties"]["clear_assignee"]["default"], false);

        let status = AgentControlVerbV1::UpdateIssueStatus
            .descriptor()
            .parameters();
        assert_eq!(
            status["properties"]["status"]["enum"],
            serde_json::json!(["Open", "InProgress", "Closed", "Cancelled"])
        );
        assert_eq!(status["properties"]["expected_row_version"]["minimum"], 1);

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
        assert_eq!(inbox["properties"]["notice_kind"]["default"], Value::Null);
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
            assert_eq!(
                resolve.validate_params(&invalid),
                Err(AgentControlParamErrorV1::params()),
                "{invalid}"
            );
        }
        let execute = AgentControlVerbV1::TopologyExecute;
        let mut spoof = fixture(execute);
        spoof["requested_by_session_id"] = serde_json::json!(id);
        assert_eq!(
            execute.validate_params(&spoof),
            Err(AgentControlParamErrorV1::params())
        );
        let mut digest = fixture(execute);
        digest["expected_digest"] = serde_json::json!("sha256:ABC");
        assert_eq!(
            execute.validate_params(&digest),
            Err(AgentControlParamErrorV1::params())
        );
        assert_eq!(
            AgentControlVerbV1::TopologyList.validate_params(&serde_json::json!({})),
            Ok(())
        );
        assert_eq!(
            AgentControlVerbV1::TopologyList.validate_params(&serde_json::json!({"limit": 33})),
            Err(AgentControlParamErrorV1::params())
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
