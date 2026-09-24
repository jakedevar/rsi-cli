//! Native `rsi_control` harness tools — in-process coordination controls
//! for agents running under the Harness (direct-API) provider.
//!
//! These exist because the Harness `shell` tool scrubs `RSI_*` from the child
//! environment (`session/harness/tools/shell.rs`), so the shell→`rsi-rpc`
//! bridge cannot carry the P0 session token here — a Harness agent could not
//! reach the tokened `Agent*` RPC verbs through a subprocess. The native tools
//! close that gap without ever exposing the token: the caller session id is
//! **bound at construction** (like `ScheduleWakeTool.origin_session_id`) and is
//! kept out of every tool's JSON input schema, so the agent can neither supply
//! nor spoof it.
//!
//! Each tool routes through the *same* guarded [`AgentControlHandle`] the
//! `Agent*` RPC verbs use (`session::agent_verbs`), so authority/scoping logic
//! lives in exactly one place (gate-pack invariant: "native tools call the same
//! guarded wrapper as the RPC verb; two enforcement layers").

use super::HarnessTool;
use crate::error::DaemonError;
use crate::session::agent_verbs::{
    AgentControlHandle, AgentSpawnChildOutcome, ProgramGuardRegistration,
};
use crate::session::harness::types::ToolResult;
use rsi_common::agent_control_schema::AgentControlVerbV1;
use rsi_common::agent_coordination::{
    AgentGetProgressParamsV1, AgentReserveSuccessorRequestV1, AgentSendMessageRequestV1,
    AgentSpawnChildRequestV1,
};
use rsi_common::harness_manager::{
    AgentManagerInboxRequestV1, AgentManagerNotifyRequestV1, AgentManagerProgressRequestV1,
    AgentManagerReplyRequestV1, AgentManagerSendRequestV1, validate_manager_message,
};
use rsi_common::rpc::{
    AgentArchiveIssueRequestV1, AgentCreateIssueParams, AgentGetIssueRequestV1,
    AgentIssueValidationV1, AgentListIssuesRequestV1, AgentRestoreIssueRequestV1,
    AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1,
};
use rsi_common::types::IssueEventPageRequestV1;
use std::path::Path;
use uuid::Uuid;

fn err(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some(msg.into()),
    }
}

/// Argument-free native program registration. Caller identity and the entire
/// scheduled-job envelope are derived server-side.
pub(crate) const PROGRAM_GUARD_INPUT_SCHEMA: &str =
    r#"{"type":"object","additionalProperties":false,"properties":{}}"#;

pub(crate) fn validate_program_guard_args(args: &serde_json::Value) -> Result<(), String> {
    match args {
        serde_json::Value::Object(fields) if fields.is_empty() => Ok(()),
        serde_json::Value::Object(_) => {
            Err("rsi_control_program_guard accepts no arguments".to_string())
        }
        _ => Err("rsi_control_program_guard arguments must be an empty object".to_string()),
    }
}

pub(crate) fn program_guard_registration_value(
    outcome: &ProgramGuardRegistration,
) -> serde_json::Value {
    match outcome {
        ProgramGuardRegistration::Registered(job) => serde_json::json!({
            "job_id": job.id,
            "wake_mode": job.wake_mode,
            "wake_session_id": job.wake_session_id,
            "deduplicated": false,
        }),
        ProgramGuardRegistration::Deduplicated(job) => serde_json::json!({
            "job_id": job.id,
            "wake_mode": job.wake_mode,
            "wake_session_id": job.wake_session_id,
            "deduplicated": true,
        }),
    }
}

/// Parse and validate the shared strict send request. Sender identity is
/// construction-bound and cannot be supplied here: `AgentSendMessageRequestV1`
/// is `deny_unknown_fields`, so an attempt to smuggle one fails to parse.
pub(crate) fn send_message_request_from_args(
    args: &serde_json::Value,
) -> Result<AgentSendMessageRequestV1, String> {
    let request: AgentSendMessageRequestV1 = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid send_message arguments: {e}"))?;
    request.validate().map_err(str::to_string)?;
    Ok(request)
}

/// Shared strict parser used by both native transports.
pub(crate) fn agent_create_issue_from_args(
    args: &serde_json::Value,
) -> Result<AgentCreateIssueParams, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("invalid issue create arguments: {e}"))
}

pub(crate) fn agent_list_issues_from_args(
    args: &serde_json::Value,
) -> Result<AgentListIssuesRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_list(args)
}

pub(crate) fn agent_get_issue_from_args(
    args: &serde_json::Value,
) -> Result<AgentGetIssueRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_get(args)
}

pub(crate) fn agent_update_issue_from_args(
    args: &serde_json::Value,
) -> Result<AgentUpdateIssueRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_update(args)
}

pub(crate) fn agent_update_issue_status_from_args(
    args: &serde_json::Value,
) -> Result<AgentUpdateIssueStatusRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_update_status(args)
}

pub(crate) fn agent_archive_issue_from_args(
    args: &serde_json::Value,
) -> Result<AgentArchiveIssueRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_archive(args)
}

pub(crate) fn agent_restore_issue_from_args(
    args: &serde_json::Value,
) -> Result<AgentRestoreIssueRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_restore(args)
}

pub(crate) fn agent_list_issue_events_from_args(
    args: &serde_json::Value,
) -> Result<IssueEventPageRequestV1, AgentIssueValidationV1> {
    crate::agent_issue_validation::decode_list_events(args)
}

pub(crate) fn agent_issue_invalid_request_json(validation: AgentIssueValidationV1) -> String {
    crate::error::agent_issue_error_json(crate::error::agent_issue_invalid_request(validation))
}

/// Parse and validate the shared strict spawn request. Caller identity is
/// construction-bound and cannot be supplied here.
pub(crate) fn spawn_request_from_args(
    args: &serde_json::Value,
) -> Result<AgentSpawnChildRequestV1, String> {
    let request: AgentSpawnChildRequestV1 = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid spawn arguments: {e}"))?;
    request.validate().map_err(str::to_string)?;
    Ok(request)
}

pub(crate) fn reserve_successor_request_from_args(
    args: &serde_json::Value,
) -> Result<AgentReserveSuccessorRequestV1, String> {
    let request: AgentReserveSuccessorRequestV1 = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid reserve_successor arguments: {e}"))?;
    request.validate().map_err(str::to_string)?;
    Ok(request)
}

pub(crate) fn progress_params_from_args(
    args: &serde_json::Value,
) -> Result<AgentGetProgressParamsV1, String> {
    if args.is_null() {
        return Ok(AgentGetProgressParamsV1::default());
    }
    serde_json::from_value(args.clone()).map_err(|e| format!("invalid progress arguments: {e}"))
}

/// Resolve an optional `session_id` argument into a target UUID, defaulting to
/// the bound caller session (target "myself"). A present-but-malformed
/// `session_id` is a caller error, never silently reinterpreted as self —
/// mirroring the `AgentGetStatus`/`AgentHalt` RPC handlers. Shared by both
/// transports.
pub(crate) fn resolve_target_id(args: &serde_json::Value, caller: Uuid) -> Result<Uuid, String> {
    match args.get("session_id") {
        None | Some(serde_json::Value::Null) => Ok(caller),
        Some(v) => v
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| {
                "'session_id' must be a valid UUID string, or omitted to target this session"
                    .to_string()
            }),
    }
}

/// Format an [`AgentSpawnChildOutcome`] into `(success, human_message)` shared
/// by both transports.
pub(crate) fn describe_spawn_outcome(outcome: &AgentSpawnChildOutcome) -> (bool, String) {
    match outcome {
        AgentSpawnChildOutcome::Accepted(result) => (
            true,
            serde_json::to_string(result).unwrap_or_else(|e| format!("serialization error: {e}")),
        ),
        AgentSpawnChildOutcome::Rejected(reason) => (false, format!("spawn rejected: {reason:?}")),
    }
}

/// `rsi_control_program_guard` — argument-free, construction-bound program
/// registration for Harness and CodexAppServer parity.
pub struct RsiControlProgramGuardTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

impl RsiControlProgramGuardTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlProgramGuardTool {
    fn name(&self) -> &str {
        "rsi_control_program_guard"
    }

    fn description(&self) -> &str {
        "Idempotently register daemon-authoritative master-orchestrate program identity. \
         This tool accepts no arguments; caller identity, same-session Resume custody, \
         timing, and row UUID are bound and derived server-side."
    }

    fn parameters_json(&self) -> &str {
        PROGRAM_GUARD_INPUT_SCHEMA
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        if let Err(error) = validate_program_guard_args(&args) {
            return err(error);
        }
        match self
            .control
            .register_bound_program_guard(self.caller_session_id)
            .await
        {
            Ok(outcome) => ToolResult {
                success: true,
                output: program_guard_registration_value(&outcome).to_string(),
                error_msg: None,
            },
            Err(error) => err(error.to_string()),
        }
    }
}

/// `rsi_control_spawn` — session-attributed child spawn. The caller (lead)
/// session is bound at construction; the child is validated and enqueued
/// through the shared spawn coordinator exactly as `AgentSpawnChild` does.
pub struct RsiControlSpawnTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

/// Strong same-Epic master baton reservation. Unlike Fresh, this preserves
/// the predecessor's authority until the daemon establishes and commits the
/// one stable successor.
pub struct RsiControlReserveSuccessorTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

impl RsiControlReserveSuccessorTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlReserveSuccessorTool {
    fn name(&self) -> &str {
        "rsi_control_reserve_successor"
    }

    fn description(&self) -> &str {
        "Reserve one daemon-authored same-Epic successor and transfer the master baton only after provider establishment. Exact retries return the original successor; caller and authority identities are bound server-side."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::ReserveSuccessor
            .descriptor()
            .parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let request = match reserve_successor_request_from_args(&args) {
            Ok(request) => request,
            Err(error) => return err(error),
        };
        match self
            .control
            .agent_reserve_successor(self.caller_session_id, request)
            .await
        {
            Ok(result) => ToolResult {
                success: true,
                output: serde_json::to_string(&result)
                    .unwrap_or_else(|error| format!("serialization error: {error}")),
                error_msg: None,
            },
            Err(error) => err(error.to_string()),
        }
    }
}

impl RsiControlSpawnTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlSpawnTool {
    fn name(&self) -> &str {
        "rsi_control_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a child agent session under the Epic you lead. Validated through \
         the same lead-identity, recursion-depth, and rate-limit checks as the \
         spawn coordinator. The spawning (caller) session is bound server-side; \
         you cannot spawn on behalf of another session."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::SpawnChild
            .descriptor()
            .parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let request = match spawn_request_from_args(&args) {
            Ok(d) => d,
            Err(e) => return err(e),
        };
        let outcome = self
            .control
            .agent_spawn_child(self.caller_session_id, request)
            .await;
        let (success, message) = describe_spawn_outcome(&outcome);
        if success {
            ToolResult {
                success: true,
                output: message,
                error_msg: None,
            }
        } else {
            err(message)
        }
    }
}

/// `rsi_control_status` — session-attributed status read. Scoped to the
/// caller's own session, a direct child, or a child of an Epic the caller leads.
pub struct RsiControlStatusTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

impl RsiControlStatusTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlStatusTool {
    fn name(&self) -> &str {
        "rsi_control_status"
    }

    fn description(&self) -> &str {
        "Read the status of a session you are authorized to observe (yourself, a \
         direct child, or a child of an Epic you lead). Omit session_id to target \
         your own session."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::GetStatus.descriptor().parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let target = match resolve_target_id(&args, self.caller_session_id) {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        match self
            .control
            .agent_get_status(self.caller_session_id, target)
            .await
        {
            Ok(session) => match serde_json::to_string(&session) {
                Ok(json) => ToolResult {
                    success: true,
                    output: json,
                    error_msg: None,
                },
                Err(e) => err(format!("failed to serialize session: {e}")),
            },
            Err(e) => err(e.to_string()),
        }
    }
}

/// `rsi_control_send_message` — durable owner→child mail (P2-03).
///
/// Routes through the same guarded [`AgentControlHandle::agent_send_message`]
/// the `AgentSendMessage` RPC verb uses, so send authority lives in exactly
/// one place. The sender is the construction-bound caller and is absent from
/// the input schema, so an agent can neither supply nor spoof it.
pub struct RsiControlSendMessageTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

impl RsiControlSendMessageTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlSendMessageTool {
    fn name(&self) -> &str {
        "rsi_control_send_message"
    }

    fn description(&self) -> &str {
        "Queue durable mail for a child agent you own (your own reserved or \
         direct child, or a child of an Epic you lead). A queued receipt proves \
         acceptance, not provider delivery. Delivery requires a supported idle \
         boundary before expiry; expiry can win first, and mail never interrupts \
         a running turn. Omitted expires_at defaults to 30 minutes after first \
         acceptance. Requires a stable idempotency_key; an exact retry returns \
         the same message ID and original deadline. You cannot message yourself, and \
         the sending session is bound server-side."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::SendMessage
            .descriptor()
            .parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let request = match send_message_request_from_args(&args) {
            Ok(request) => request,
            Err(e) => return err(e),
        };
        match self
            .control
            .agent_send_message(self.caller_session_id, request)
            .await
        {
            Ok(receipt) => match serde_json::to_string(&receipt) {
                Ok(output) => ToolResult {
                    success: true,
                    output,
                    error_msg: None,
                },
                Err(e) => err(format!("failed to serialize receipt: {e}")),
            },
            Err(e) => err(e.to_string()),
        }
    }
}

/// `rsi_control_progress` — one durable, bounded cohort snapshot.
pub struct RsiControlProgressTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

impl RsiControlProgressTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlProgressTool {
    fn name(&self) -> &str {
        "rsi_control_progress"
    }

    fn description(&self) -> &str {
        "Read one bounded durable snapshot for the child cohort you are authorized to observe, including status counts, event cursors, freshness, watch state, and message counts."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::GetProgress
            .descriptor()
            .parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let params = match progress_params_from_args(&args) {
            Ok(params) => params,
            Err(e) => return err(e),
        };
        match self
            .control
            .agent_get_progress(self.caller_session_id, &params.session_ids)
            .await
        {
            Ok(result) => match serde_json::to_string(&result) {
                Ok(output) => ToolResult {
                    success: true,
                    output,
                    error_msg: None,
                },
                Err(e) => err(format!("failed to serialize progress: {e}")),
            },
            Err(e) => err(e.to_string()),
        }
    }
}

/// `rsi_control_halt` — session-attributed interrupt. Same scoping as
/// `rsi_control_status`; delegates to the shared interrupt path.
pub struct RsiControlHaltTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

/// `rsi_control_create_issue` — construction-bound manual issue creation.
pub struct RsiControlCreateIssueTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
}

impl RsiControlCreateIssueTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlCreateIssueTool {
    fn name(&self) -> &str {
        "rsi_control_create_issue"
    }

    fn description(&self) -> &str {
        "Create one durable local issue follow-up. The creator is bound to this session; use a stable idempotency_key for safe retries."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::CreateIssue
            .descriptor()
            .parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let params = match agent_create_issue_from_args(&args) {
            Ok(params) => params,
            Err(e) => return err(e),
        };
        match self
            .control
            .agent_create_issue(self.caller_session_id, params)
            .await
        {
            Ok(result) => match serde_json::to_string(&result) {
                Ok(output) => ToolResult {
                    success: true,
                    output,
                    error_msg: None,
                },
                Err(e) => err(format!("failed to serialize issue result: {e}")),
            },
            Err(e) => err(e.to_string()),
        }
    }
}

/// One construction-bound wrapper type for each V95 lead-only Issue control
/// operation. The operation is fixed at registration; JSON can never select a
/// different method or supply caller/project identity.
#[derive(Clone, Copy)]
pub enum IssueControlToolKind {
    List,
    Get,
    Update,
    UpdateStatus,
    Archive,
    Restore,
    ListEvents,
}

impl IssueControlToolKind {
    pub(crate) const fn verb(self) -> AgentControlVerbV1 {
        match self {
            Self::List => AgentControlVerbV1::ListIssues,
            Self::Get => AgentControlVerbV1::GetIssue,
            Self::Update => AgentControlVerbV1::UpdateIssue,
            Self::UpdateStatus => AgentControlVerbV1::UpdateIssueStatus,
            Self::Archive => AgentControlVerbV1::ArchiveIssue,
            Self::Restore => AgentControlVerbV1::RestoreIssue,
            Self::ListEvents => AgentControlVerbV1::ListIssueEvents,
        }
    }
}

pub struct RsiControlIssueTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
    kind: IssueControlToolKind,
}

impl RsiControlIssueTool {
    pub fn new(
        control: AgentControlHandle,
        caller_session_id: Uuid,
        kind: IssueControlToolKind,
    ) -> Self {
        Self {
            control,
            caller_session_id,
            kind,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlIssueTool {
    fn name(&self) -> &str {
        match self.kind {
            IssueControlToolKind::List => "rsi_control_list_issues",
            IssueControlToolKind::Get => "rsi_control_get_issue",
            IssueControlToolKind::Update => "rsi_control_update_issue",
            IssueControlToolKind::UpdateStatus => "rsi_control_update_issue_status",
            IssueControlToolKind::Archive => "rsi_control_archive_issue",
            IssueControlToolKind::Restore => "rsi_control_restore_issue",
            IssueControlToolKind::ListEvents => "rsi_control_list_issue_events",
        }
    }

    fn description(&self) -> &str {
        match self.kind {
            IssueControlToolKind::List => {
                "List bounded local Issues in the project owned by the Epic you currently lead."
            }
            IssueControlToolKind::Get => {
                "Read one local Issue in the project owned by the Epic you currently lead."
            }
            IssueControlToolKind::Update => {
                "CAS-update active Issue content as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            IssueControlToolKind::UpdateStatus => {
                "CAS-update one Issue status as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            IssueControlToolKind::Archive => {
                "Archive a terminal Issue with row-version CAS as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            IssueControlToolKind::Restore => {
                "Restore an archived Issue with row-version CAS as the current owning-Epic lead or manager with issue-coordinate authority."
            }
            IssueControlToolKind::ListEvents => {
                "Read bounded immutable Issue audit history as the current owning-Epic lead."
            }
        }
    }

    fn parameters_json(&self) -> &str {
        self.kind.verb().descriptor().parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let result = match self.kind {
            IssueControlToolKind::List => match agent_list_issues_from_args(&args) {
                Ok(params) => {
                    self.control
                        .agent_list_issues(self.caller_session_id, params)
                        .await
                }
                Err(validation) => return err(agent_issue_invalid_request_json(validation)),
            }
            .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json)),
            IssueControlToolKind::Get => match agent_get_issue_from_args(&args) {
                Ok(params) => {
                    self.control
                        .agent_get_issue(self.caller_session_id, params)
                        .await
                }
                Err(validation) => return err(agent_issue_invalid_request_json(validation)),
            }
            .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json)),
            IssueControlToolKind::Update => match agent_update_issue_from_args(&args) {
                Ok(params) => {
                    self.control
                        .agent_update_issue(self.caller_session_id, params)
                        .await
                }
                Err(validation) => return err(agent_issue_invalid_request_json(validation)),
            }
            .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json)),
            IssueControlToolKind::UpdateStatus => {
                match agent_update_issue_status_from_args(&args) {
                    Ok(params) => {
                        self.control
                            .agent_update_issue_status(self.caller_session_id, params)
                            .await
                    }
                    Err(validation) => return err(agent_issue_invalid_request_json(validation)),
                }
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
            }
            IssueControlToolKind::Archive => match agent_archive_issue_from_args(&args) {
                Ok(params) => {
                    self.control
                        .agent_archive_issue(self.caller_session_id, params)
                        .await
                }
                Err(validation) => return err(agent_issue_invalid_request_json(validation)),
            }
            .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json)),
            IssueControlToolKind::Restore => match agent_restore_issue_from_args(&args) {
                Ok(params) => {
                    self.control
                        .agent_restore_issue(self.caller_session_id, params)
                        .await
                }
                Err(validation) => return err(agent_issue_invalid_request_json(validation)),
            }
            .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json)),
            IssueControlToolKind::ListEvents => match agent_list_issue_events_from_args(&args) {
                Ok(params) => {
                    self.control
                        .agent_list_issue_events(self.caller_session_id, params)
                        .await
                }
                Err(validation) => return err(agent_issue_invalid_request_json(validation)),
            }
            .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json)),
        };
        match result {
            Ok(value) => match serde_json::to_string(&value) {
                Ok(output) => ToolResult {
                    success: true,
                    output,
                    error_msg: None,
                },
                Err(_) => err(crate::error::agent_issue_error_json(
                    crate::error::agent_issue_error(
                        rsi_common::rpc::AgentIssueErrorCodeV1::StorageFailure,
                        None,
                        None,
                    ),
                )),
            },
            Err(error) => err(crate::error::agent_issue_error_json(error)),
        }
    }
}

/// Scoped manager and assigned-reviewer tools; appointment is never an agent capability.
#[derive(Clone, Copy, Debug)]
pub enum ManagerControlToolKind {
    Progress,
    Inbox,
    Send,
    Reply,
    Notify,
    Inspect,
    Update,
    SubmitReviewReceipt,
    Control,
    PrepareControl,
    CommitPreparedControl,
    GetAction,
    WorkView,
}

impl ManagerControlToolKind {
    pub(crate) const ALL: [Self; 13] = [
        Self::Progress,
        Self::Inbox,
        Self::Send,
        Self::Reply,
        Self::Notify,
        Self::Inspect,
        Self::Update,
        Self::SubmitReviewReceipt,
        Self::Control,
        Self::PrepareControl,
        Self::CommitPreparedControl,
        Self::GetAction,
        Self::WorkView,
    ];

    pub(crate) const fn verb(self) -> AgentControlVerbV1 {
        match self {
            Self::Progress => AgentControlVerbV1::ManagerProgress,
            Self::Inbox => AgentControlVerbV1::ManagerInbox,
            Self::Send => AgentControlVerbV1::ManagerSend,
            Self::Reply => AgentControlVerbV1::ManagerReply,
            Self::Notify => AgentControlVerbV1::ManagerNotify,
            Self::Inspect => AgentControlVerbV1::ManagerInspect,
            Self::Update => AgentControlVerbV1::ManagerUpdate,
            Self::SubmitReviewReceipt => AgentControlVerbV1::SubmitReviewReceipt,
            Self::Control => AgentControlVerbV1::ManagerControl,
            Self::PrepareControl => AgentControlVerbV1::ManagerPrepareControl,
            Self::CommitPreparedControl => AgentControlVerbV1::ManagerCommitPreparedControl,
            Self::GetAction => AgentControlVerbV1::ManagerGetAction,
            Self::WorkView => AgentControlVerbV1::ManagerWorkView,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Progress => "rsi_control_manager_progress",
            Self::Inbox => "rsi_control_manager_inbox",
            Self::Send => "rsi_control_manager_send",
            Self::Reply => "rsi_control_manager_reply",
            Self::Notify => "rsi_control_manager_notify",
            Self::Inspect => "rsi_control_manager_inspect",
            Self::Update => "rsi_control_manager_update",
            Self::SubmitReviewReceipt => "rsi_control_submit_review_receipt",
            Self::Control => "rsi_control_manager_control",
            Self::PrepareControl => "rsi_control_manager_prepare_control",
            Self::CommitPreparedControl => "rsi_control_manager_commit_prepared_control",
            Self::GetAction => "rsi_control_manager_get_action",
            Self::WorkView => "rsi_control_manager_work_view",
        }
    }
}

fn parse_manager_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Value,
) -> crate::error::Result<T> {
    // Serde diagnostics may contain unknown keys, raw values or paths.
    rsi_common::harness_manager::decode_manager_request(args)
        .map_err(|code| DaemonError::InvalidParam(code.into()))
}

/// Both native transports bind the caller at registration and use this adapter
/// to reach the same guarded service as the token-authenticated RPC handlers.
pub(crate) async fn execute_manager_tool(
    control: &AgentControlHandle,
    caller: Uuid,
    kind: ManagerControlToolKind,
    args: serde_json::Value,
) -> crate::error::Result<serde_json::Value> {
    match kind {
        ManagerControlToolKind::Inspect => {
            let request: rsi_common::harness_manager_v2::AgentManagerInspectRequestV2 =
                parse_manager_args(args)?;
            control
                .agent_manager_inspect(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Update => {
            let request: rsi_common::harness_manager_v2::AgentManagerUpdateRequestV2 =
                parse_manager_args(args)?;
            control
                .agent_manager_update(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::SubmitReviewReceipt => {
            let request: rsi_common::harness_manager_v2::AgentSubmitReviewReceiptRequestV1 =
                parse_manager_args(args)?;
            control
                .agent_submit_review_receipt(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Control => {
            let request: rsi_common::harness_manager_v2::AgentManagerControlRequestV2 =
                parse_manager_args(args)?;
            control
                .agent_manager_control(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::PrepareControl => {
            let request: rsi_common::harness_manager_v2::AgentManagerPrepareControlRequestV2 =
                parse_manager_args(args)?;
            control
                .agent_manager_prepare_control(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::CommitPreparedControl => {
            let request: rsi_common::harness_manager_v2::AgentManagerCommitPreparedControlRequestV2 =
                parse_manager_args(args)?;
            control
                .agent_manager_commit_prepared_control(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::GetAction => {
            let request: rsi_common::harness_manager_v2::AgentManagerGetActionRequestV2 =
                parse_manager_args(args)?;
            control
                .agent_manager_get_action(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::WorkView => {
            let request: rsi_common::harness_manager::AgentManagerWorkViewRequestV1 =
                parse_manager_args(args)?;
            control
                .agent_manager_work_view(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Progress => {
            let request: AgentManagerProgressRequestV1 = parse_manager_args(args)?;
            control
                .agent_manager_progress(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Inbox => {
            let request: AgentManagerInboxRequestV1 = parse_manager_args(args)?;
            request
                .validate()
                .map_err(|code| DaemonError::InvalidParam(code.into()))?;
            control
                .agent_manager_inbox(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Send => {
            let request: AgentManagerSendRequestV1 = parse_manager_args(args)?;
            validate_manager_message(request.epic_id, &request.message, &request.idempotency_key)
                .map_err(|code| DaemonError::InvalidParam(code.into()))?;
            control
                .agent_manager_send(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Reply => {
            let request: AgentManagerReplyRequestV1 = parse_manager_args(args)?;
            validate_manager_message(
                request.request_id,
                &request.message,
                &request.idempotency_key,
            )
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
            control
                .agent_manager_reply(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
        ManagerControlToolKind::Notify => {
            let request: AgentManagerNotifyRequestV1 = parse_manager_args(args)?;
            request
                .validate()
                .map_err(|code| DaemonError::InvalidParam(code.into()))?;
            control
                .agent_manager_notify(caller, request)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(DaemonError::Json))
        }
    }
}

pub struct RsiControlManagerTool {
    control: AgentControlHandle,
    caller_session_id: Uuid,
    kind: ManagerControlToolKind,
}

impl RsiControlManagerTool {
    pub fn new(
        control: AgentControlHandle,
        caller_session_id: Uuid,
        kind: ManagerControlToolKind,
    ) -> Self {
        Self {
            control,
            caller_session_id,
            kind,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlManagerTool {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn description(&self) -> &str {
        self.kind.verb().descriptor().description
    }

    fn parameters_json(&self) -> &str {
        self.kind.verb().descriptor().parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        match execute_manager_tool(&self.control, self.caller_session_id, self.kind, args).await {
            Ok(value) => ToolResult {
                success: true,
                output: value.to_string(),
                error_msg: None,
            },
            Err(error) => err(error.to_string()),
        }
    }
}

impl RsiControlHaltTool {
    pub fn new(control: AgentControlHandle, caller_session_id: Uuid) -> Self {
        Self {
            control,
            caller_session_id,
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for RsiControlHaltTool {
    fn name(&self) -> &str {
        "rsi_control_halt"
    }

    fn description(&self) -> &str {
        "Interrupt a session you are authorized to control (yourself, a direct \
         child, or a child of an Epic you lead). Omit session_id to halt your own \
         session."
    }

    fn parameters_json(&self) -> &str {
        AgentControlVerbV1::Halt.descriptor().parameters_json()
    }

    async fn execute(&self, args: serde_json::Value, _working_dir: &Path) -> ToolResult {
        let target = match resolve_target_id(&args, self.caller_session_id) {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        match self
            .control
            .agent_halt(self.caller_session_id, target)
            .await
        {
            Ok(()) => ToolResult {
                success: true,
                output: format!("halt requested for session {target}"),
                error_msg: None,
            },
            Err(e) => err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manager_native_tools_reject_identity_and_redact_malformed_args() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        for kind in ManagerControlToolKind::ALL {
            let reference = Uuid::new_v4();
            let valid = match kind {
                ManagerControlToolKind::Progress
                | ManagerControlToolKind::Inbox
                | ManagerControlToolKind::Inspect
                | ManagerControlToolKind::WorkView => {
                    serde_json::json!({})
                }
                ManagerControlToolKind::Update => {
                    serde_json::json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"one","change":{"update":"handoff","summary":"Ready","next_actions":[]}})
                }
                ManagerControlToolKind::SubmitReviewReceipt => {
                    serde_json::json!({"assignment_id":reference,"verdict":"accepted","findings":[],"idempotency_key":"receipt"})
                }
                ManagerControlToolKind::Control => {
                    serde_json::json!({"fence":{"scope_version":1,"policy_version":1},"idempotency_key":"one","operation":{"action":"create_container","kind":"Group","name":"Group","tags":[]}})
                }
                ManagerControlToolKind::PrepareControl => {
                    serde_json::json!({"operation":{"action":"resume_lead","epic_id":reference,"message":"continue"}})
                }
                ManagerControlToolKind::CommitPreparedControl => {
                    serde_json::json!({"prepared_id":reference,"target_digest":format!("sha256:{}", "a".repeat(64)),"idempotency_key":"commit"})
                }
                ManagerControlToolKind::GetAction => {
                    serde_json::json!({"operation_id":reference})
                }
                ManagerControlToolKind::Send => serde_json::json!({
                    "epic_id": reference, "message": "evidence?", "idempotency_key": "request-1"
                }),
                ManagerControlToolKind::Reply => serde_json::json!({
                    "request_id": reference, "message": "checks passed", "idempotency_key": "reply-1"
                }),
                ManagerControlToolKind::Notify => serde_json::json!({
                    "message": "checks passed", "idempotency_key": "notify-1"
                }),
            };
            let tool = RsiControlManagerTool::new(control.clone(), caller, kind);
            for field in [
                "caller_session_id",
                "sender_session_id",
                "recipient_session_id",
                "project_id",
                "/private/unknown-field",
            ] {
                let mut forged = valid.clone();
                forged[field] = serde_json::json!("/private/value token=secret");
                let result = tool.execute(forged, Path::new("/tmp")).await;
                assert!(!result.success, "tool: {}", tool.name());
                assert_eq!(
                    result.error_msg,
                    Some(DaemonError::InvalidParam("manager_invalid_request".into()).to_string())
                );
                assert!(result.output.is_empty());
            }
            for malformed in [
                serde_json::Value::Null,
                serde_json::json!([]),
                serde_json::json!("/private/value"),
            ] {
                let error = execute_manager_tool(&control, caller, kind, malformed)
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, DaemonError::InvalidParam(code) if code == "manager_invalid_request")
                );
            }
        }
    }

    #[tokio::test]
    async fn manager_native_tools_validate_page_and_message_bounds_before_service() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        for args in [
            serde_json::json!({"limit": 0}),
            serde_json::json!({"limit": 33}),
            serde_json::json!({"after_sequence": -1}),
        ] {
            let error = execute_manager_tool(&control, caller, ManagerControlToolKind::Inbox, args)
                .await
                .unwrap_err();
            assert!(
                matches!(error, DaemonError::InvalidParam(code) if code == "manager_invalid_inbox_page")
            );
        }
        for (kind, id_field) in [
            (ManagerControlToolKind::Send, "epic_id"),
            (ManagerControlToolKind::Reply, "request_id"),
        ] {
            for message in [
                " ".to_string(),
                "é".repeat(4097),
                "NUL\0content".to_string(),
            ] {
                let mut args = serde_json::json!({"message": message, "idempotency_key": "one"});
                args[id_field] = serde_json::json!(Uuid::new_v4());
                let error = execute_manager_tool(&control, caller, kind, args)
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, DaemonError::InvalidParam(code) if code == "manager_invalid_message")
                );
            }
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn d05_program_run_authority_fields_are_absent_from_native_schemas() {
        for schema in rsi_common::agent_control_schema::agent_control_catalog_v1()
            .iter()
            .map(|descriptor| descriptor.parameters_json())
            .chain(std::iter::once(PROGRAM_GUARD_INPUT_SCHEMA))
        {
            let value: serde_json::Value =
                serde_json::from_str(schema).expect("valid native schema");
            let encoded = value.to_string();
            for forbidden in [
                "controller_epoch",
                "lease_generation",
                "claim_generation",
                "program_run_id",
                "expected_run_version",
                "expected_idea_version",
            ] {
                assert!(
                    !encoded.contains(forbidden),
                    "native schema exposed {forbidden}"
                );
            }
        }
    }

    /// The bound caller session id is a private field set at construction and
    /// is NOT present in any tool's JSON input schema — the agent can neither
    /// supply nor spoof it. (Native-tool session-binding ratchet.)
    #[test]
    fn caller_session_id_is_never_in_the_input_schema() {
        for schema in rsi_common::agent_control_schema::agent_control_catalog_v1()
            .iter()
            .map(|descriptor| descriptor.parameters_json())
            .chain(std::iter::once(PROGRAM_GUARD_INPUT_SCHEMA))
        {
            let v: serde_json::Value = serde_json::from_str(schema).expect("valid schema json");
            let props = &v["properties"];
            assert!(
                props.get("caller_session_id").is_none(),
                "caller_session_id must never be an input property"
            );
            assert!(
                props.get("origin_session_id").is_none(),
                "origin_session_id must never be an input property"
            );
            assert!(
                props.get("caller").is_none(),
                "caller must never be an input property"
            );
        }
    }

    #[test]
    fn successor_schema_uses_baton_specific_language() {
        let successor = AgentControlVerbV1::ReserveSuccessor
            .descriptor()
            .parameters_json();
        let spawn = AgentControlVerbV1::SpawnChild
            .descriptor()
            .parameters_json();
        assert_ne!(successor, spawn);
        assert!(successor.contains("Successor session kind"));
        assert!(successor.contains("successor's initial prompt"));
        assert!(!successor.contains("Child session kind"));
        assert!(!successor.contains("child's initial prompt"));
    }

    #[test]
    fn spawn_schema_accepts_an_explicit_child_provider() {
        let request = spawn_request_from_args(&serde_json::json!({
            "kind": "Task",
            "provider": "Claude",
            "model": "claude-sonnet-5",
            "query": "review this change",
            "idempotency_key": "cross-provider-child"
        }))
        .expect("provider must be accepted by the shared spawn schema");
        assert_eq!(
            request.provider,
            Some(rsi_common::types::SessionProvider::Claude)
        );
        assert_eq!(request.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn spawn_schema_accepts_explicit_pioneer_child_provider() {
        let request = spawn_request_from_args(&serde_json::json!({
            "kind": "Task",
            "provider": "Pioneer",
            "model": "claude-sonnet-5",
            "query": "review this change",
            "idempotency_key": "pioneer-child"
        }))
        .expect("Pioneer must be accepted by the shared spawn schema");
        assert_eq!(
            request.provider,
            Some(rsi_common::types::SessionProvider::Pioneer)
        );
        assert_eq!(request.model.as_deref(), Some("claude-sonnet-5"));
    }

    fn test_control_handle() -> AgentControlHandle {
        use crate::session::spawn_coordinator::SpawnCoordinator;
        use std::collections::HashMap;
        use std::sync::Arc;
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open_in_memory().unwrap(),
        ));
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let coordinator = Arc::new(SpawnCoordinator::new(tx));
        AgentControlHandle::new(
            active,
            completed,
            store,
            Arc::new(crate::bus::EventBus::new(16)),
            coordinator,
        )
    }

    #[tokio::test]
    async fn agent_issue_harness_parser_and_authority_failures_use_safe_envelopes() {
        use rsi_common::rpc::{
            AgentIssueErrorCodeV1, AgentIssueErrorV1, AgentIssueValidationClassV1 as Class,
            AgentIssueValidationFieldV1 as Field,
        };

        let caller = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        for (kind, args, expected_class, expected_field) in [
            (
                IssueControlToolKind::List,
                serde_json::json!({"limit": "many"}),
                Class::InvalidField,
                Some(Field::Limit),
            ),
            (
                IssueControlToolKind::Get,
                serde_json::json!({"issue_id": issue_id, "project_id/secret": "never"}),
                Class::UnknownField,
                None,
            ),
            (
                IssueControlToolKind::Update,
                serde_json::json!({"issue_id": issue_id}),
                Class::MissingField,
                Some(Field::ExpectedRowVersion),
            ),
            (
                IssueControlToolKind::UpdateStatus,
                serde_json::json!({
                    "issue_id": issue_id,
                    "status": 7,
                    "expected_row_version": 1,
                    "idempotency_key": "status-harness"
                }),
                Class::InvalidField,
                Some(Field::Status),
            ),
            (
                IssueControlToolKind::Archive,
                serde_json::json!({
                    "issue_id": issue_id,
                    "expected_row_version": 1,
                    "idempotency_key": 7
                }),
                Class::InvalidField,
                Some(Field::IdempotencyKey),
            ),
            (
                IssueControlToolKind::Restore,
                serde_json::json!([]),
                Class::InvalidShape,
                None,
            ),
            (
                IssueControlToolKind::ListEvents,
                serde_json::json!({
                    "issue_id": issue_id,
                    "after_sequence": "zero"
                }),
                Class::InvalidField,
                Some(Field::AfterSequence),
            ),
        ] {
            let tool = RsiControlIssueTool::new(test_control_handle(), caller, kind);
            let malformed = tool.execute(args, std::path::Path::new("/tmp")).await;
            let encoded = malformed.error_msg.as_deref().unwrap();
            let malformed: AgentIssueErrorV1 = serde_json::from_str(encoded).unwrap();
            assert_eq!(malformed.code, AgentIssueErrorCodeV1::InvalidRequest);
            let validation = malformed.validation.expect("bounded validation hint");
            assert_eq!(validation.class, expected_class);
            assert_eq!(validation.field, expected_field);
            assert!(!encoded.contains("project_id/secret"));
            assert!(!encoded.contains("never"));
        }

        let tool =
            RsiControlIssueTool::new(test_control_handle(), caller, IssueControlToolKind::Get);
        let denied = tool
            .execute(
                serde_json::json!({"issue_id":Uuid::new_v4()}),
                std::path::Path::new("/tmp"),
            )
            .await;
        let denied: AgentIssueErrorV1 =
            serde_json::from_str(denied.error_msg.as_deref().unwrap()).unwrap();
        assert_eq!(denied.code, AgentIssueErrorCodeV1::AuthorityDenied);
        assert_eq!(denied.validation, None);
    }

    /// The tool binds the caller session id at construction and routes it —
    /// never an arg — through the guarded [`AgentControlHandle`] verb. With
    /// `session_id` omitted the tool targets the BOUND caller (self); since the
    /// caller is absent from the empty maps the guarded verb returns
    /// "not found", proving both the binding and that the call reached the
    /// shared authority path. (Native-tool session-binding ratchet.)
    #[tokio::test]
    async fn status_tool_binds_caller_and_routes_through_guarded_verb() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        let tool = RsiControlStatusTool::new(control, caller);

        let r = tool
            .execute(serde_json::json!({}), std::path::Path::new("/tmp"))
            .await;
        assert!(!r.success);
        assert!(
            r.error_msg.unwrap().to_lowercase().contains("not found"),
            "self-target should route the bound caller through the guarded verb"
        );
    }

    #[tokio::test]
    async fn progress_tool_binds_caller_and_routes_through_guarded_verb() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        let tool = RsiControlProgressTool::new(control, caller);
        let result = tool
            .execute(serde_json::json!({}), std::path::Path::new("/tmp"))
            .await;
        assert!(!result.success);
        assert!(
            result
                .error_msg
                .expect("missing bound caller must fail")
                .to_lowercase()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn program_guard_tool_is_argument_free_and_routes_bound_caller() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        let tool = RsiControlProgramGuardTool::new(control, caller);

        let injected = tool
            .execute(
                serde_json::json!({ "caller_session_id": caller }),
                std::path::Path::new("/tmp"),
            )
            .await;
        assert!(!injected.success);
        assert_eq!(
            injected.error_msg.as_deref(),
            Some("rsi_control_program_guard accepts no arguments")
        );

        let routed = tool
            .execute(serde_json::json!({}), std::path::Path::new("/tmp"))
            .await;
        assert!(!routed.success);
        assert!(
            routed
                .error_msg
                .expect("missing bound caller must fail")
                .to_lowercase()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn progress_tool_rejects_257_duplicate_entries_before_authorization() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        let tool = RsiControlProgressTool::new(control, caller);
        let result = tool
            .execute(
                serde_json::json!({ "session_ids": vec![caller; 257] }),
                std::path::Path::new("/tmp"),
            )
            .await;
        assert!(!result.success);
        assert_eq!(
            result.error_msg.as_deref(),
            Some("agent_progress_cohort_too_large:257>256")
        );
    }

    /// A malformed `session_id` is a caller error — it must NOT silently
    /// fall back to self-targeting (mirrors the AgentGetStatus/AgentHalt RPC
    /// handlers).
    /// P2-03: the send schema forbids every sender-identity spelling, so the
    /// advertised contract cannot invite an agent to name its own sender, and
    /// the strict parser refuses one even if an agent guesses a field name.
    #[test]
    fn send_message_schema_and_parser_reject_every_sender_field() {
        let schema = AgentControlVerbV1::SendMessage.descriptor().parameters();
        assert_eq!(schema["additionalProperties"], false);
        let props = &schema["properties"];
        for forbidden in [
            "owner_session_id",
            "sender_session_id",
            "caller_session_id",
            "from_session_id",
            "session_token",
        ] {
            assert!(
                props.get(forbidden).is_none(),
                "send schema must never advertise {forbidden}"
            );
        }
        // Exactly the four contract fields, no more.
        let advertised: std::collections::BTreeSet<&str> = props
            .as_object()
            .expect("object properties")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            advertised,
            [
                "target_session_id",
                "message",
                "idempotency_key",
                "expires_at"
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
        );

        // The parser is the real enforcement: `deny_unknown_fields`.
        let target = Uuid::new_v4();
        for smuggled in ["owner_session_id", "sender_session_id", "caller_session_id"] {
            let args = serde_json::json!({
                "target_session_id": target,
                "message": "hi",
                "idempotency_key": "k",
                smuggled: Uuid::new_v4(),
            });
            assert!(
                send_message_request_from_args(&args).is_err(),
                "parser accepted smuggled {smuggled}"
            );
        }
        // Bounds are enforced by the shared validator, not by the schema text.
        for bad in [
            serde_json::json!({ "target_session_id": target, "message": "", "idempotency_key": "k" }),
            serde_json::json!({ "target_session_id": target, "message": "hi", "idempotency_key": "" }),
            serde_json::json!({ "target_session_id": Uuid::nil(), "message": "hi", "idempotency_key": "k" }),
            serde_json::json!({ "target_session_id": target, "message": "hi" }),
        ] {
            assert!(
                send_message_request_from_args(&bad).is_err(),
                "parser accepted out-of-contract request: {bad}"
            );
        }
        assert!(
            send_message_request_from_args(&serde_json::json!({
                "target_session_id": target, "message": "hi", "idempotency_key": "k",
            }))
            .is_ok()
        );
    }

    /// The native tool routes through the same guarded handle as the RPC verb,
    /// so an unauthorized target is refused identically here.
    #[tokio::test]
    async fn send_message_tool_denies_an_unauthorized_target() {
        let control = test_control_handle();
        let caller = Uuid::new_v4();
        let tool = RsiControlSendMessageTool::new(control, caller);
        let result = tool
            .execute(
                serde_json::json!({
                    "target_session_id": caller,
                    "message": "hi",
                    "idempotency_key": "k",
                }),
                std::path::Path::new("/tmp"),
            )
            .await;
        assert!(!result.success, "self-send must be refused natively too");
        assert!(
            result
                .error_msg
                .expect("denial message")
                .starts_with("agent_message_target_not_authorized"),
            "the native transport must surface the same stable class"
        );
    }

    #[tokio::test]
    async fn halt_tool_rejects_malformed_session_id() {
        let control = test_control_handle();
        let tool = RsiControlHaltTool::new(control, Uuid::new_v4());
        let r = tool
            .execute(
                serde_json::json!({ "session_id": "not-a-uuid" }),
                std::path::Path::new("/tmp"),
            )
            .await;
        assert!(!r.success);
        assert!(r.error_msg.unwrap().contains("valid UUID"));
    }
}
