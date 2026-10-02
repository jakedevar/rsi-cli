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
    SendSatelliteMessage,
    GetDaemonInfo,
    RequestDeploy,
    QueryFailureSignatures,
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
    RsiControlManagerWorkView,
    RsiControlTopologyUpsert,
    RsiControlTopologyList,
    RsiControlTopologyExecute,
    RsiControlTopologyGetExecution,
    RsiControlTopologyInterrupt,
    RsiControlTopologyResolveAttempt,
    RsiControlReadSessionEvents,
    RsiControlQueryFailureSignatures,
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
            Self::RsiControlManagerWorkView => "rsi_control_manager_work_view",
            Self::RsiControlTopologyUpsert => "rsi_control_topology_upsert",
            Self::RsiControlTopologyList => "rsi_control_topology_list",
            Self::RsiControlTopologyExecute => "rsi_control_topology_execute",
            Self::RsiControlTopologyGetExecution => "rsi_control_topology_get_execution",
            Self::RsiControlTopologyInterrupt => "rsi_control_topology_interrupt",
            Self::RsiControlTopologyResolveAttempt => "rsi_control_topology_resolve_attempt",
            Self::RsiControlReadSessionEvents => "rsi_control_read_session_events",
            Self::RsiControlQueryFailureSignatures => "rsi_control_query_failure_signatures",
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
            Self::RsiControlManagerWorkView => AgentControlVerbV1::ManagerWorkView,
            Self::RsiControlTopologyUpsert => AgentControlVerbV1::TopologyUpsert,
            Self::RsiControlTopologyList => AgentControlVerbV1::TopologyList,
            Self::RsiControlTopologyExecute => AgentControlVerbV1::TopologyExecute,
            Self::RsiControlTopologyGetExecution => AgentControlVerbV1::TopologyGetExecution,
            Self::RsiControlTopologyInterrupt => AgentControlVerbV1::TopologyInterrupt,
            Self::RsiControlTopologyResolveAttempt => AgentControlVerbV1::TopologyResolveAttempt,
            Self::RsiControlReadSessionEvents => AgentControlVerbV1::ReadSessionEvents,
            Self::RsiControlQueryFailureSignatures => AgentControlVerbV1::QueryFailureSignatures,
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
            Self::SendSatelliteMessage => decode!(
                crate::satellite_dispatch::AgentSendSatelliteMessageRequestV1,
                |r: &crate::satellite_dispatch::AgentSendSatelliteMessageRequestV1| r.validate()
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
const READ_SESSION_EVENTS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["session_id"],"properties":{"session_id":{"type":"string","format":"uuid","description":"A session you may observe: your own child, a child of an Epic you lead, or a session in your manager scope"},"after_sequence":{"type":["integer","null"],"minimum":0,"default":null,"description":"Forward page: events with a greater sequence, ascending. Omit for the tail (newest events, ascending)."},"limit":{"type":["integer","null"],"minimum":1,"maximum":100,"default":20},"event_types":{"type":["array","null"],"maxItems":8,"items":{"type":"string","enum":["Message","ToolUse","ToolResult","System","Thinking","Compressed","CompletionGate"]},"default":null,"description":"Only these event types"},"max_bytes":{"type":["integer","null"],"minimum":1024,"maximum":262144,"default":32768,"description":"Page byte budget; content, tool_input and metadata are also clipped per event"}}}"#;
const REQUEST_DEPLOY_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["sha","idempotency_key"],"properties":{"sha":{"type":"string","minLength":40,"maxLength":40,"pattern":"^[0-9a-f]{40}$","description":"Full lowercase commit SHA the binaries were built from"},"binaries_dir":{"type":["string","null"],"default":null,"description":"Absolute directory of freshly built binaries (rsid required); under the sandbox base, ~/.rsi/staging or ~/.cargo/shared-target"},"build":{"type":["boolean","null"],"default":null,"description":"Reserved: build at sha through the job path; refused deploy_build_not_supported"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Required stable dedup key; an exact retry returns the same deploy"},"max_wait_secs":{"type":["integer","null"],"minimum":1,"maximum":3600,"default":null,"description":"Bound on the wait for a quiet point; default 900"},"peer_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Deploy on this paired satellite over the hub link instead of on this daemon; binaries_dir is then a path on the satellite. Confirm with AgentGetDaemonInfo satellites"}}}"#;
const QUERY_FAILURE_SIGNATURES_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"test_id":{"type":["string","null"],"minLength":1,"maxLength":512,"default":null,"description":"Exact libtest/nextest test id, e.g. session::launch::tests::x"},"digest":{"type":["string","null"],"pattern":"^[0-9a-f]{64}$","default":null,"description":"sha256 failure digest (rsi-known-failure block prints it); both fields must match when both are set"}}}"#;
const GET_DAEMON_INFO_SCHEMA: &str =
    r#"{"type":"object","additionalProperties":false,"properties":{}}"#;
const GET_PROVIDER_STATUS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"provider":{"type":["string","null"],"enum":["claude","codex","pioneer","openrouter","bedrock","local","antigravity","codex_app_server","harness","anthropic","openai",null],"default":null,"description":"Provider to report; omit for every provider"}}}"#;
const TARGET_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Target session UUID; omit to target this session"}}}"#;
const WAKE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["message","mode"],"properties":{"message":{"type":"string","description":"Prompt to run when the wake fires"},"in_seconds":{"type":["integer","null"],"minimum":1,"default":null,"description":"Fire this many seconds from now; mutually exclusive with at"},"at":{"type":["string","null"],"format":"date-time","default":null,"description":"RFC3339 absolute fire time; mutually exclusive with in_seconds"},"name":{"type":["string","null"],"default":null,"description":"Optional human-readable job name"},"every_seconds":{"type":["integer","null"],"minimum":1,"default":null,"description":"Optional recurring interval in seconds"},"mode":{"type":"string","enum":["fresh","resume","on_terminal","program_guard","when"],"description":"Required: fresh is a consumed, best-effort root launch after this session is terminal and transfers no hierarchy or lead authority; use AgentReserveSuccessor for master turnover. resume re-invokes this session with context; on_terminal arms a terminal watch; program_guard registers daemon-authoritative program identity; when arms ONE resume wake that the daemon fires when the `when` predicate is true (no model turn is spent polling) and carries each job's id, name, state, exit code and refusal"},"watch_session_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Watched subject UUID; requires mode on_terminal and cannot steer the caller-bound wake target"},"when":{"type":["object","null"],"default":null,"additionalProperties":false,"description":"Required with mode when (and only then): exactly one predicate the daemon evaluates. jobs_terminal fires once every listed AgentSubmitJob job you own is terminal (submit them with wake none for one wake per batch); sha_on_rolling fires once that commit is an ancestor of origin/rolling in your repository (polled). in_seconds, at and every_seconds are not accepted with mode when","properties":{"jobs_terminal":{"type":"array","items":{"type":"string","format":"uuid"},"minItems":1,"maxItems":32,"description":"Job ids you own; an unknown or foreign id is refused at scheduling time"},"sha_on_rolling":{"type":"string","pattern":"^[0-9a-f]{40}$","description":"Full lowercase 40-hex commit id"}}},"timeout_seconds":{"type":["integer","null"],"minimum":1,"maximum":604800,"default":null,"description":"mode when only: fire with timed_out true after this many seconds if the predicate is still pending, so you are never stranded"}}}"#;
const CANCEL_WAKE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"job_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Job id returned by AgentScheduleWake; must be one of your own jobs. Provide exactly one of job_id or name"},"name":{"type":["string","null"],"minLength":1,"maxLength":256,"default":null,"description":"Name of your own scheduled job(s) to disable. Provide exactly one of job_id or name"}}}"#;
const LIST_WAKES_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"include_disabled":{"type":"boolean","default":false,"description":"Also return disabled (cancelled, fired or suspended) jobs"},"limit":{"type":["integer","null"],"minimum":1,"maximum":256,"default":64,"description":"Maximum jobs returned, soonest fire first"}}}"#;
const CREATE_ISSUE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["title","idempotency_key"],"properties":{"title":{"type":"string","minLength":1,"maxLength":512},"body":{"type":"string","maxLength":65536,"default":""},"priority":{"type":["integer","null"],"minimum":1,"maximum":4,"default":null},"labels":{"type":"array","maxItems":64,"items":{"type":"string","maxLength":128},"default":[]},"assignee":{"type":["string","null"],"maxLength":256,"default":null},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const LIST_ISSUES_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"status":{"type":["string","null"],"enum":["Open","InProgress","Closed","Cancelled",null],"default":null},"archive":{"type":"string","enum":["Active","Archived","All"],"default":"Active"},"cursor":{"type":["object","null"],"additionalProperties":false,"required":["display_number","issue_id"],"properties":{"display_number":{"type":"integer","minimum":1},"issue_id":{"type":"string","format":"uuid"}},"default":null},"limit":{"type":["integer","null"],"minimum":1,"maximum":256,"default":64},"ready":{"type":"boolean","default":false,"description":"When true, include only open, active Issues with no open or in-progress blockers"},"order":{"type":"string","enum":["asc","desc"],"default":"asc","description":"Sort by display number; desc lists newest first. The cursor works in both orders"},"title_contains":{"type":["string","null"],"minLength":1,"maxLength":128,"default":null,"description":"Case-insensitive substring filter over Issue titles"}}}"#;
const GET_ISSUE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"issue_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Issue UUID; set exactly one of issue_id and display_number"},"display_number":{"type":["integer","null"],"minimum":1,"default":null,"description":"Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"}}}"#;
const UPDATE_ISSUE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["expected_row_version","idempotency_key"],"properties":{"issue_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Issue UUID; set exactly one of issue_id and display_number"},"display_number":{"type":["integer","null"],"minimum":1,"default":null,"description":"Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128},"title":{"type":["string","null"],"maxLength":512,"default":null},"body":{"type":["string","null"],"maxLength":65536,"default":null},"labels":{"type":["array","null"],"maxItems":64,"items":{"type":"string","maxLength":128},"default":null},"priority":{"type":["integer","null"],"minimum":1,"maximum":4,"default":null},"clear_priority":{"type":"boolean","default":false},"assignee":{"type":["string","null"],"maxLength":256,"default":null},"clear_assignee":{"type":"boolean","default":false}}}"#;
const UPDATE_ISSUE_STATUS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["status","expected_row_version","idempotency_key"],"properties":{"issue_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Issue UUID; set exactly one of issue_id and display_number"},"display_number":{"type":["integer","null"],"minimum":1,"default":null,"description":"Project-scoped display number (the N in #N); set exactly one of issue_id and display_number"},"status":{"type":"string","enum":["Open","InProgress","Closed","Cancelled"]},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const ISSUE_CAS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["issue_id","expected_row_version","idempotency_key"],"properties":{"issue_id":{"type":"string","format":"uuid"},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}}}"#;
const LIST_ISSUE_EVENTS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["issue_id"],"properties":{"issue_id":{"type":"string","format":"uuid"},"after_sequence":{"type":"integer","minimum":0,"default":0},"limit":{"type":["integer","null"],"minimum":1,"maximum":256,"default":64}}}"#;

const CONTINUE_CHILD_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["target_session_id","query","expected_tip_session_id","expected_event_sequence"],"properties":{"target_session_id":{"type":"string","format":"uuid","description":"The child session to continue; the caller itself is refused"},"query":{"type":"string","minLength":1,"maxLength":262144,"description":"Continuation prompt delivered as the child's next turn"},"expected_tip_session_id":{"type":"string","format":"uuid","description":"Required staleness fence: the lineage tip the caller last observed"},"expected_event_sequence":{"type":"integer","minimum":0,"description":"Required staleness fence: MAX(sequence) of the tip's conversation events as last observed"},"expected_custody_generation":{"type":["integer","null"],"minimum":1,"default":null,"description":"Optional staleness fence: the sandbox custody generation as last observed; must match exactly including absence"}}}"#;
const ARCHIVE_CHILD_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["target_session_id","expected_tip_session_id","expected_event_sequence"],"properties":{"target_session_id":{"type":"string","format":"uuid","description":"Terminal child to archive; current Epic lead only"},"expected_tip_session_id":{"type":"string","format":"uuid","description":"Required staleness fence: last observed lineage tip"},"expected_event_sequence":{"type":"integer","minimum":0,"description":"Required staleness fence: last observed tip event sequence"},"expected_custody_generation":{"type":["integer","null"],"minimum":1,"default":null,"description":"Optional staleness fence: last observed sandbox custody generation"}}}"#;

const MANAGER_PROGRESS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"after_epic_id":{"type":["string","null"],"format":"uuid","description":"Continue after next_after_epic_id from the previous page."},"limit":{"type":["integer","null"],"minimum":1,"maximum":64,"default":32}}}"#;
const MANAGER_INBOX_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"after_sequence":{"type":"integer","minimum":0,"default":0},"limit":{"type":"integer","minimum":1,"maximum":32,"default":32},"request_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Optional recorded request to read within your live manager or feature-lead scope"}}}"#;
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

const SUBMIT_JOB_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["kind","params"],"properties":{"kind":{"type":"string","enum":["test","build","landing","cloud_gate","cloud_sweep"]},"params":{"type":"object","description":"test: {shard,filterset?} or {package,filters[],lib_only?}; build: {command:check|build,package|workspace,all_targets?,release?}; landing/cloud_gate: {accepted:40-hex sha,test_filters?:[PACKAGE=FILTER]}; cloud_sweep: {sha:40-hex current rolling tip}. Typed fields only; no command line."},"name":{"type":"string","minLength":1,"maxLength":80},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Optional replay key"},"worktree":{"type":"string","description":"Appointed manager only: another worktree of your own repository to run in; default is your sandbox"},"wake":{"type":["string","null"],"enum":["owner","none",null],"default":"owner","description":"owner (default): one resume wake to you when this job settles. none: no per-job wake; the job still settles and AgentGetJob/AgentListJobs return its result. Submit a batch with none, then arm one AgentScheduleWake mode when with when.jobs_terminal so you are woken once"}}}"#;

const GET_JOB_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["job_id"],"properties":{"job_id":{"type":"string","format":"uuid"}}}"#;

const LIST_JOBS_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"limit":{"type":"integer","minimum":1,"maximum":100}}}"#;

const SEND_SATELLITE_MESSAGE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["peer_id","remote_session_id","message","idempotency_key"],"properties":{"peer_id":{"type":"string","description":"Operator-registered satellite peer id"},"remote_session_id":{"type":"string","description":"Session id on the satellite, inside the operator-declared scope"},"message":{"type":"string","minLength":1,"maxLength":16384,"description":"Message text; queued until the remote session is idle"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"},"expires_at":{"type":"string","description":"Optional RFC3339 expiry, at most 24 hours ahead; default 30 minutes"}}}"#;
const ENQUEUE_LANDING_SOURCE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["source_commit","idempotency_key"],"properties":{"source_commit":{"type":"string","pattern":"^[0-9a-f]{40}$","description":"Full lowercase 40-hex accepted source commit reachable from your sandbox"},"test_filters":{"type":"array","maxItems":32,"items":{"type":"string","minLength":3,"maxLength":256,"description":"PACKAGE=FILTER; the gate is a compile check plus the named tests"},"default":[]},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"}}}"#;

const TOPOLOGY_UPSERT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["name","definition","scope","idempotency_key"],"properties":{"name":{"type":"string","minLength":1,"maxLength":128,"description":"Topology name, unique per owner"},"definition":{"type":"object","required":["nodes","edges"],"properties":{"nodes":{"type":"array","items":{"type":"object"}},"edges":{"type":"array","items":{"type":"object"}},"until":{"type":["object","null"]}},"description":"TopologyDefinition: nodes (id, kind, label, prereqs, params incl. typed step, custody, explicit provider/model/effort), edges, until; the daemon validates it and returns diagnostics"},"scope":{"type":"string","enum":["epic","manager"],"description":"epic: owned by one Epic (leads may only use this); manager: owned by the current manager"},"epic_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Target Epic for a manager epic-scoped upsert; a lead's Epic is daemon-derived and, if given, must match"},"expected_revision":{"type":["integer","null"],"minimum":1,"default":null,"description":"CAS fence to revise an existing topology; omit to create"},"validate_only":{"type":"boolean","default":false,"description":"Report diagnostics without writing"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; reuse only for identical content"}}}"#;
const TOPOLOGY_LIST_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"scope":{"type":["string","null"],"enum":["epic","manager",null],"default":null},"epic_id":{"type":["string","null"],"format":"uuid","default":null,"description":"Narrow to one Epic in your scope"},"include_executions":{"type":"boolean","default":false},"cursor":{"type":["string","null"],"maxLength":128,"default":null,"description":"next_cursor from the previous page"},"limit":{"type":["integer","null"],"minimum":1,"maximum":32,"default":null}}}"#;
const TOPOLOGY_EXECUTE_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["topology_id","expected_digest","epic_id","idempotency_key"],"properties":{"topology_id":{"type":"string","format":"uuid"},"expected_digest":{"type":"string","pattern":"^sha256:[0-9a-f]{64}$","description":"definition_digest returned by list or upsert"},"epic_id":{"type":"string","format":"uuid","description":"Epic the execution runs under; must be in your scope"},"inputs":{"type":["object","null"],"default":null},"base_commit":{"type":["string","null"],"pattern":"^[0-9a-fA-F]{40}$","default":null,"description":"Full 40-hex base; omitted resolves origin/rolling once at acceptance"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL; a replay returns deduplicated=true"}}}"#;
const TOPOLOGY_GET_EXECUTION_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["execution_id"],"properties":{"execution_id":{"type":"string","format":"uuid"},"after_sequence":{"type":["integer","null"],"minimum":0,"default":null,"description":"next_sequence from the previous page"},"limit":{"type":["integer","null"],"minimum":1,"maximum":64,"default":null}}}"#;
const TOPOLOGY_INTERRUPT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["execution_id","expected_row_version","idempotency_key"],"properties":{"execution_id":{"type":"string","format":"uuid"},"expected_row_version":{"type":"integer","minimum":1,"description":"row_version observed via get_execution; refresh after stale_version"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"}}}"#;
const TOPOLOGY_RESOLVE_ATTEMPT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["execution_id","attempt_id","action","expected_row_version","idempotency_key"],"properties":{"execution_id":{"type":"string","format":"uuid"},"attempt_id":{"type":"string","format":"uuid","description":"Attempt blocked on preserved work"},"action":{"type":"string","enum":["inspect","accept","retry","discard"],"description":"discard is manager (Automation) only; a lead is refused"},"expected_row_version":{"type":"integer","minimum":1},"idempotency_key":{"type":"string","minLength":1,"maxLength":128,"description":"Replay key, at most 128 UTF-8 bytes, without NUL"},"confirm_preserved_commit":{"type":["string","null"],"pattern":"^[0-9a-fA-F]{40}$","default":null,"description":"Full 40-hex preserved_commit; required iff action=discard, refused otherwise"}}}"#;

static AGENT_CONTROL_CATALOG_V1: LazyLock<[AgentControlDescriptorV1; 53]> = LazyLock::new(|| {
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
            description: "CAS-update active Issue content (target by issue_id or display_number, exactly one) as the current owning-Epic lead or manager with issue-coordinate authority.",
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
            description: "Retrieve durable manager requests/replies and settle exact manager notices in your live scope; retrieval does not reply, answer a decision, clear a question, grant approval, or prove provider acceptance.",
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
            description: "Read one manager action receipt within the authenticated current manager's project and logical manager scope.",
            parameters_json: manager_v2::GET_ACTION.as_str(),
            native_tool: Some(NativeAgentControlToolV1::RsiControlManagerGetAction),
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
            description: "Read a bounded page of a session's conversation events (tail or after_sequence), with final_message and terminal_reason, for your own child, a child of an Epic you lead, or a session in your manager scope. Read-only; content is clipped per event and per page.",
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
            description: "Run a long test, build, landing or cloud-gate operation as a daemon-owned durable job in your sandbox. It survives your turn ending, your session and a daemon restart; one resume wake carries the typed result (wake none suppresses that per-job wake: submit a batch with wake none, then arm one AgentScheduleWake mode when with when.jobs_terminal for a single wake). Dispatch it and end your turn.",
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
            verb: AgentControlVerbV1::SendSatelliteMessage,
            method: "AgentSendSatelliteMessage",
            description: "Queue one message for an idle session on a paired satellite host (current appointed manager only; the operator must enable dispatch and declare the target in scope). Delivery waits until the remote session is idle and never interrupts; `queued` is acceptance, not delivery. Every refusal is the same target_not_authorized.",
            parameters_json: SEND_SATELLITE_MESSAGE_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::GetDaemonInfo,
            method: "AgentGetDaemonInfo",
            description: "Read the running hub daemon's identity and health for the current appointed manager or Epic lead: embedded build SHA, sha256 of the running binary, start time, schema version, disk free for the data dir and sandbox base, 1/5/15 load average, and supervisor mode. The appointed manager also gets `satellites`: each enabled paired satellite read over the link now (build_sha, binary_sha256, started_at, schema_version, supervisor_mode, last_deploy, or reachable:false). Secret-free: never returns an environment value. RPC-only.",
            parameters_json: GET_DAEMON_INFO_SCHEMA,
            native_tool: None,
        },
        AgentControlDescriptorV1 {
            verb: AgentControlVerbV1::RequestDeploy,
            method: "AgentRequestDeploy",
            description: "Ask the daemon to deploy already-built binaries (current appointed manager holding the operator-granted Deploy capability, Execute mode, not paused). The daemon stages and verifies them, waits for a quiet point (no lander, job or scoped worker mid-turn; bounded), swaps them in, restarts under rsid-supervisor.sh with its environment intact and wakes you once with the outcome. With peer_id it does the same on a paired satellite over the link (#1017; the operator must enable dispatch and declare the satellite's scope): the satellite runs its own deploy flow, nothing is stored on the hub, and you confirm through AgentGetDaemonInfo satellites (build_sha, last_deploy) and your own resume wake. RPC-only.",
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
        }
    }

    #[test]
    fn catalog_is_closed_ordered_unique_and_valid() {
        let catalog = agent_control_catalog_v1();
        assert_eq!(catalog.len(), 53);
        assert_eq!(
            catalog.iter().map(|entry| entry.method).collect::<Vec<_>>(),
            [
                "AgentGetAuthorityCatalog",
                "AgentSpawnChild",
                "AgentReserveSuccessor",
                "AgentGetProgress",
                "AgentSendMessage",
                "AgentGetStatus",
                "AgentHalt",
                "AgentContinueChild",
                "AgentArchiveChild",
                "AgentScheduleWake",
                "AgentCancelWake",
                "AgentListWakes",
                "AgentCreateIssue",
                "AgentListIssues",
                "AgentGetIssue",
                "AgentUpdateIssue",
                "AgentUpdateIssueStatus",
                "AgentArchiveIssue",
                "AgentRestoreIssue",
                "AgentListIssueEvents",
                "AgentManagerProgress",
                "AgentManagerInbox",
                "AgentManagerSend",
                "AgentManagerReply",
                "AgentManagerNotify",
                "AgentManagerInspect",
                "AgentManagerUpdate",
                "AgentSubmitReviewReceipt",
                "AgentManagerControl",
                "AgentManagerPrepareControl",
                "AgentManagerCommitPreparedControl",
                "AgentManagerGetAction",
                "AgentManagerWorkView",
                "AgentManagerDelegateNode",
                "AgentManagerEscalate",
                "AgentManagerListEscalations",
                "AgentManagerResolveEscalation",
                "AgentTopologyUpsert",
                "AgentTopologyList",
                "AgentTopologyExecute",
                "AgentTopologyGetExecution",
                "AgentTopologyInterrupt",
                "AgentTopologyResolveAttempt",
                "AgentEnqueueLandingSource",
                "AgentReadSessionEvents",
                "AgentGetProviderStatus",
                "AgentSubmitJob",
                "AgentGetJob",
                "AgentListJobs",
                "AgentSendSatelliteMessage",
                "AgentGetDaemonInfo",
                "AgentRequestDeploy",
                "AgentQueryFailureSignatures",
            ]
        );
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
                "AgentSendSatelliteMessage",
                "AgentGetDaemonInfo",
                "AgentRequestDeploy",
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
                    "idempotency_key",
                    "labels",
                    "priority",
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
                &["after_sequence", "limit", "request_id"],
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
                    "wake",
                    "worktree",
                ],
                &["kind", "params"],
            ),
            (AgentControlVerbV1::GetJob, &["job_id"], &["job_id"]),
            (AgentControlVerbV1::ListJobs, &["limit"], &[]),
            (
                AgentControlVerbV1::EnqueueLandingSource,
                &["idempotency_key", "source_commit", "test_filters"],
                &["source_commit", "idempotency_key"],
            ),
            (AgentControlVerbV1::GetProviderStatus, &["provider"], &[]),
            (AgentControlVerbV1::GetDaemonInfo, &[], &[]),
            (
                AgentControlVerbV1::QueryFailureSignatures,
                &["digest", "test_id"],
                &[],
            ),
            (
                AgentControlVerbV1::RequestDeploy,
                &[
                    "binaries_dir",
                    "build",
                    "idempotency_key",
                    "max_wait_secs",
                    "peer_id",
                    "sha",
                ],
                &["sha", "idempotency_key"],
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
