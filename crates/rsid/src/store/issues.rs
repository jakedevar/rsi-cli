//! Local issue tracker persistence (C1 / V72): `issues` + `issue_deps`.
//!
//! CRUD, dependency-edge ops, and the ready-work query. No hard-delete
//! function on purpose (DB rule 2): `Cancelled` is the logical delete;
//! the `ON DELETE CASCADE` on `issue_deps` stays as defensive schema truth.

#[path = "issue_events_manager_actor.rs"]
pub(in crate::store) mod manager_actor_migration;

use super::Store;
use super::daemon_settings::{
    C5AutofilePending, C5TransitionError, C5TransitionResult, commit_c5_transaction,
    source_session_id_from_c5_pending_key,
};
use super::row_mappers::parse_timestamp;
use crate::error::{DaemonError, Result, agent_issue_error, issue_workspace_error};
use chrono::{SecondsFormat, Utc};
use rsi_common::issue_workspace::{
    ArchiveIssueRequestV1, CreateIssueV2RequestV1, GetIssueInProjectRequestV1,
    IssueDependencyCursorV1, IssueDependencyDirectionV1, IssueDependencyItemV1,
    IssueDependencyMutationRequestV1, IssueDependencyMutationResultV1, IssueDependencyPageV1,
    IssueWorkspaceCursorV1, IssueWorkspaceErrorCodeV1, IssueWorkspacePageV1,
    IssueWorkspaceProvenanceV1, IssueWorkspaceReadinessFilterV1, IssueWorkspaceReadinessV1,
    IssueWorkspaceRowV1, IssueWorkspaceSortV1, ListIssueDependenciesRequestV1,
    ListIssueEventsV2RequestV1, ListIssuesPageRequestV1, NullablePatchV1,
    OperatorIssueMutationResultV1, PageDirectionV1, RestoreIssueRequestV1, UpdateIssueRequestV1,
    UpdateIssueStatusV2RequestV1, operator_issue_create_id, operator_issue_mutation_event_id,
};
use rsi_common::program_runs::canonical_program_run_json;
use rsi_common::rpc::{
    AgentArchiveIssueRequestV1, AgentGetIssueRequestV1, AgentIssueMutationResultV1,
    AgentListIssuesRequestV1, AgentRestoreIssueRequestV1, AgentUpdateIssueRequestV1,
    AgentUpdateIssueStatusRequestV1,
};
use rsi_common::types::{
    AgentGetIssueResultV1, AgentIssueDependencyRefV1, Issue, IssueActorKindV1,
    IssueArchiveFilterV1, IssueContentPatchV1, IssueCreateSemanticV1, IssueDep,
    IssueEventOperationV1, IssueEventPageRequestV1, IssueEventPageV1, IssueEventV1, IssueFilter,
    IssueListCursorV1, IssuePageV1, IssueSemanticOperationV1, IssueSemanticRequestV1,
    IssueSourceFindingRef, IssueStatus, IssueStatusTransitionErrorV1, IssueUpdate, NewIssue,
    Recurrence, ScheduledJob, WakeMode,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

/// Result of deterministic issue creation. The existing row is returned on a
/// replay; callers decide whether a mismatched replay is a conflict.
#[derive(Debug, Clone)]
pub(crate) struct IdempotentIssueCreate {
    pub issue: Issue,
    pub deduplicated: bool,
    pub create_fields_match: bool,
}

/// Durable result of one no-idle settlement transaction. Project-backed
/// sessions atomically own both effects; project-less sessions explicitly
/// settle wake-only instead of pretending an issue was filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MasterNoIdleStoreRecovery {
    WakeAndIssue {
        issue_id: Uuid,
        wake_deduplicated: bool,
        issue_deduplicated: bool,
    },
    ProjectlessWakeOnly {
        wake_deduplicated: bool,
    },
}

#[cfg(test)]
thread_local! {
    static MASTER_NO_IDLE_FAIL_AFTER_WAKE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn master_no_idle_test_fail_after_wake(count: usize) {
    MASTER_NO_IDLE_FAIL_AFTER_WAKE.with(|remaining| remaining.set(count));
}

#[cfg(test)]
fn master_no_idle_fault_after_wake() -> Result<()> {
    let injected = MASTER_NO_IDLE_FAIL_AFTER_WAKE.with(|remaining| {
        let count = remaining.get();
        if count == 0 {
            false
        } else {
            remaining.set(count - 1);
            true
        }
    });
    if injected {
        return Err(DaemonError::Store(
            "master_no_idle_transient:injected_after_wake".into(),
        ));
    }
    Ok(())
}

#[cfg(not(test))]
fn master_no_idle_fault_after_wake() -> Result<()> {
    Ok(())
}

/// Runtime Issue-writer boundaries used to prove projection/event/key
/// rollback. The hook is thread-local so parallel tests cannot consume each
/// other's injection.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueWriteFault {
    AfterProjection,
    AfterEvent,
    BeforeCommit,
}

#[cfg(test)]
thread_local! {
    static ISSUE_WRITE_FAULT: std::cell::Cell<Option<IssueWriteFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn fail_next_issue_write(fault: IssueWriteFault) {
    ISSUE_WRITE_FAULT.with(|armed| armed.set(Some(fault)));
}

#[cfg(test)]
fn issue_write_fault(boundary: IssueWriteFault) -> Result<()> {
    let injected = ISSUE_WRITE_FAULT.with(|armed| {
        if armed.get() == Some(boundary) {
            armed.set(None);
            true
        } else {
            false
        }
    });
    if injected {
        return Err(DaemonError::Store(format!(
            "issue_write_injected:{boundary:?}"
        )));
    }
    Ok(())
}

pub(crate) fn issue_projection_written() -> Result<()> {
    #[cfg(test)]
    issue_write_fault(IssueWriteFault::AfterProjection)?;
    Ok(())
}

pub(crate) fn issue_write_before_commit() -> Result<()> {
    #[cfg(test)]
    issue_write_fault(IssueWriteFault::BeforeCommit)?;
    Ok(())
}

/// Result of the serialized C5 admission/settlement transaction.  A marker is
/// never admitted by a stale snapshot: the transaction verifies it is still
/// present, its source is durably Failed, and no persisted successor exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum C5SettlementOutcome {
    Committed { deduplicated: bool },
    Suppressed,
    StaleMarker,
}

/// Column list shared by every `SELECT … FROM issues`.
pub(super) const ISSUE_COLUMNS: &str = "id, project_id, display_number, title, body, status, priority, labels, \
     created_by_session_id, assignee, idea_id, source_event_id, source_finding_ref, created_at, updated_at, closed_at, \
     row_version, archived_at";

const QUALIFIED_ISSUE_COLUMNS: &str = "i.id, i.project_id, i.display_number, i.title, i.body, i.status, i.priority, i.labels, \
     i.created_by_session_id, i.assignee, i.idea_id, i.source_event_id, i.source_finding_ref, i.created_at, i.updated_at, \
     i.closed_at, i.row_version, i.archived_at";

const ISSUE_EVENT_COLUMNS: &str = "id, project_id, issue_id, sequence, operation, actor_kind, actor_session_id, \
     owning_epic_id, actor_label, expected_row_version, resulting_row_version, idempotency_key, request_fingerprint, \
     request_json, result_json, occurred_at";

/// Cycle-walk depth cap for `add_issue_dep` (beads `AddDependencyInTx` prior art).
const DEP_CYCLE_MAX_DEPTH: i64 = 100;
const AGENT_ISSUE_DEPENDENCY_LIMIT: usize = 256;
const AGENT_ISSUE_DEPENDENCY_FETCH_LIMIT: usize = AGENT_ISSUE_DEPENDENCY_LIMIT + 1;

/// RFC3339 nanosecond timestamp string (house timestamp rule).
fn now_nanos() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Intermediate row for reading issues from the database.
pub(super) struct IssueRow {
    id_str: String,
    project_id_str: String,
    display_number: i64,
    title: String,
    body: String,
    status_str: String,
    priority: Option<u8>,
    labels_json: String,
    created_by_str: Option<String>,
    assignee: Option<String>,
    idea_id_str: Option<String>,
    source_event_id_str: Option<String>,
    source_finding_ref_str: Option<String>,
    created_at_str: String,
    updated_at_str: String,
    closed_at_str: Option<String>,
    row_version: i64,
    archived_at_str: Option<String>,
}

impl IssueRow {
    pub(super) fn into_issue(self) -> Result<Issue> {
        let id = Uuid::parse_str(&self.id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid issue UUID: {}", e)))?;
        let project_id = Uuid::parse_str(&self.project_id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid issue project UUID: {}", e)))?;
        let created_by_session_id = self
            .created_by_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid issue creator UUID: {}", e)))?;
        let status = IssueStatus::parse(&self.status_str).map_err(DaemonError::Store)?;
        let idea_id = self
            .idea_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid linked Idea UUID: {e}")))?;
        let source_event_id = self
            .source_event_id_str
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid issue source event UUID: {e}")))?;
        let source_finding_ref = self
            .source_finding_ref_str
            .map(IssueSourceFindingRef::parse)
            .transpose()
            .map_err(DaemonError::Store)?;
        let labels: Vec<String> = serde_json::from_str(&self.labels_json)
            .map_err(|e| DaemonError::Store(format!("Invalid issue labels JSON: {}", e)))?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;
        let updated_at = parse_timestamp(&self.updated_at_str).map_err(DaemonError::Store)?;
        let closed_at = self
            .closed_at_str
            .as_deref()
            .map(parse_timestamp)
            .transpose()
            .map_err(DaemonError::Store)?;
        let archived_at = self
            .archived_at_str
            .as_deref()
            .map(parse_timestamp)
            .transpose()
            .map_err(DaemonError::Store)?;
        if self.row_version < 1 {
            return Err(DaemonError::Store("Invalid Issue row version".to_string()));
        }

        Ok(Issue {
            id,
            project_id,
            display_number: self.display_number,
            title: self.title,
            body: self.body,
            status,
            priority: self.priority,
            labels,
            created_by_session_id,
            assignee: self.assignee,
            idea_id,
            source_event_id,
            source_finding_ref,
            created_at,
            updated_at,
            closed_at,
            row_version: self.row_version,
            archived_at,
        })
    }
}

pub(super) fn map_issue_row(row: &rusqlite::Row) -> rusqlite::Result<IssueRow> {
    Ok(IssueRow {
        id_str: row.get(0)?,
        project_id_str: row.get(1)?,
        display_number: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        status_str: row.get(5)?,
        priority: row.get(6)?,
        labels_json: row.get(7)?,
        created_by_str: row.get(8)?,
        assignee: row.get(9)?,
        idea_id_str: row.get(10)?,
        source_event_id_str: row.get(11)?,
        source_finding_ref_str: row.get(12)?,
        created_at_str: row.get(13)?,
        updated_at_str: row.get(14)?,
        closed_at_str: row.get(15)?,
        row_version: row.get(16)?,
        archived_at_str: row.get(17)?,
    })
}

fn map_dep_row(row: &rusqlite::Row) -> rusqlite::Result<(String, String, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn dep_from_row(row: (String, String, String, String)) -> Result<IssueDep> {
    let (project_id_str, issue_id_str, depends_on_str, created_at_str) = row;
    let project_id = Uuid::parse_str(&project_id_str)
        .map_err(|e| DaemonError::Store(format!("Invalid dep project UUID: {e}")))?;
    let issue_id = Uuid::parse_str(&issue_id_str)
        .map_err(|e| DaemonError::Store(format!("Invalid dep issue UUID: {}", e)))?;
    let depends_on_id = Uuid::parse_str(&depends_on_str)
        .map_err(|e| DaemonError::Store(format!("Invalid dep blocker UUID: {}", e)))?;
    let created_at = parse_timestamp(&created_at_str).map_err(DaemonError::Store)?;
    Ok(IssueDep {
        project_id,
        issue_id,
        depends_on_id,
        created_at,
    })
}

#[derive(Debug, Clone)]
pub(super) enum IssueWriteActor {
    Operator {
        label: String,
    },
    /// Operator mutation whose durable receipt identity is derived from the
    /// request key. The key is deliberately not persisted in `issue_events`;
    /// only the deterministic event UUID carries retry identity.
    OperatorIdempotent {
        label: String,
        event_id: Uuid,
    },
    Session {
        session_id: Uuid,
        owning_epic_id: Option<Uuid>,
        idempotency_key: String,
    },
    /// The current appointed manager acting through `IssueCoordinate`. It is
    /// recorded as itself, never under an owning Epic it does not have.
    Manager {
        session_id: Uuid,
        idempotency_key: String,
    },
    System {
        label: String,
    },
}

impl IssueWriteActor {
    fn event_fields(
        &self,
    ) -> (
        IssueActorKindV1,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        Option<String>,
    ) {
        match self {
            Self::Operator { label } | Self::OperatorIdempotent { label, .. } => (
                IssueActorKindV1::Operator,
                None,
                None,
                Some(label.clone()),
                None,
            ),
            Self::Session {
                session_id,
                owning_epic_id,
                idempotency_key,
            } => (
                IssueActorKindV1::Session,
                Some(*session_id),
                *owning_epic_id,
                None,
                Some(idempotency_key.clone()),
            ),
            Self::Manager {
                session_id,
                idempotency_key,
            } => (
                IssueActorKindV1::Manager,
                Some(*session_id),
                None,
                None,
                Some(idempotency_key.clone()),
            ),
            Self::System { label } => (
                IssueActorKindV1::System,
                None,
                None,
                Some(label.clone()),
                None,
            ),
        }
    }
}

/// Which guarded Issue authority the caller resolved to. The two paths never
/// share fields: a manager has no owning Epic and no lead generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentIssueActor {
    /// The current lead of the caller's owning Epic at `lead_generation`.
    Lead { epic_id: Uuid, lead_generation: i64 },
    /// The current appointed manager holding the V2 `IssueCoordinate` grant
    /// bound to the live appointment `scope_version`.
    Manager { scope_version: i64 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AgentIssueAuthority {
    pub caller_session_id: Uuid,
    pub project_id: Uuid,
    pub(super) actor: AgentIssueActor,
}

fn canonical_timestamp(timestamp: chrono::DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn bind_value(
    values: &mut Vec<rusqlite::types::Value>,
    value: impl Into<rusqlite::types::Value>,
) -> String {
    values.push(value.into());
    format!("?{}", values.len())
}

fn issue_workspace_cursor(issue: &Issue, sort: IssueWorkspaceSortV1) -> IssueWorkspaceCursorV1 {
    match sort {
        IssueWorkspaceSortV1::UpdatedDesc => IssueWorkspaceCursorV1::UpdatedDesc {
            updated_at: issue.updated_at,
            display_number: issue.display_number,
            issue_id: issue.id,
        },
        IssueWorkspaceSortV1::DisplayNumberAsc => IssueWorkspaceCursorV1::DisplayNumberAsc {
            display_number: issue.display_number,
            issue_id: issue.id,
        },
        IssueWorkspaceSortV1::PriorityAsc => IssueWorkspaceCursorV1::PriorityAsc {
            priority: issue.priority,
            display_number: issue.display_number,
            issue_id: issue.id,
        },
    }
}

fn dependency_cursor(item: &IssueDependencyItemV1) -> IssueDependencyCursorV1 {
    IssueDependencyCursorV1 {
        display_number: item.related_issue.display_number,
        issue_id: item.related_issue.id,
    }
}

fn issue_event_id(issue_id: Uuid, actor: &IssueWriteActor) -> Uuid {
    match actor {
        IssueWriteActor::OperatorIdempotent { event_id, .. } => *event_id,
        IssueWriteActor::Session {
            session_id,
            idempotency_key,
            ..
        }
        | IssueWriteActor::Manager {
            session_id,
            idempotency_key,
        } => Uuid::new_v5(
            &issue_id,
            format!("rsi.issue.event/v1/{session_id}/{idempotency_key}").as_bytes(),
        ),
        _ => Uuid::new_v4(),
    }
}

fn baseline_issue_event_id(issue_id: Uuid) -> Uuid {
    Uuid::new_v5(&issue_id, b"rsi.issue.event/v97/baseline_imported")
}

fn operator_issue_create_event_id(issue_id: Uuid) -> Uuid {
    Uuid::new_v5(&issue_id, b"rsi.issue.operator-create-event/v1")
}

fn issue_row_conversion_error(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    )
}

fn issue_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IssueEventV1> {
    let id_str: String = row.get(0)?;
    let project_id_str: String = row.get(1)?;
    let issue_id_str: String = row.get(2)?;
    let operation: String = row.get(4)?;
    let actor_kind: String = row.get(5)?;
    let actor_session_id: Option<String> = row.get(6)?;
    let owning_epic_id: Option<String> = row.get(7)?;
    let request_json: String = row.get(13)?;
    let result_json: String = row.get(14)?;
    let occurred_at: String = row.get(15)?;
    let id = Uuid::parse_str(&id_str).map_err(issue_row_conversion_error)?;
    let project_id = Uuid::parse_str(&project_id_str).map_err(issue_row_conversion_error)?;
    let issue_id = Uuid::parse_str(&issue_id_str).map_err(issue_row_conversion_error)?;
    let actor_session_id = actor_session_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(issue_row_conversion_error)?;
    let owning_epic_id = owning_epic_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(issue_row_conversion_error)?;
    let operation = IssueEventOperationV1::parse(&operation).map_err(issue_row_conversion_error)?;
    let actor_kind = IssueActorKindV1::parse(&actor_kind).map_err(issue_row_conversion_error)?;
    let request: IssueSemanticRequestV1 =
        serde_json::from_str(&request_json).map_err(issue_row_conversion_error)?;
    request.validate().map_err(issue_row_conversion_error)?;
    if request
        .canonical_json()
        .map_err(issue_row_conversion_error)?
        != request_json
    {
        return Err(issue_row_conversion_error(
            "Issue event request JSON is not canonical",
        ));
    }
    let issue: Issue = serde_json::from_str(&result_json).map_err(issue_row_conversion_error)?;
    if canonical_program_run_json(&issue).map_err(issue_row_conversion_error)? != result_json {
        return Err(issue_row_conversion_error(
            "Issue event result JSON is not canonical",
        ));
    }
    let resulting_row_version: i64 = row.get(10)?;
    let request_fingerprint: String = row.get(12)?;
    if request.fingerprint().map_err(issue_row_conversion_error)? != request_fingerprint {
        return Err(issue_row_conversion_error(
            "Issue event request fingerprint is invalid",
        ));
    }
    let occurred_at_value = parse_timestamp(&occurred_at).map_err(issue_row_conversion_error)?;
    if canonical_timestamp(occurred_at_value) != occurred_at {
        return Err(issue_row_conversion_error(
            "Issue event occurred_at is not canonical nanosecond RFC3339",
        ));
    }
    if issue.id != issue_id
        || issue.project_id != project_id
        || issue.row_version != resulting_row_version
    {
        return Err(issue_row_conversion_error(
            "Issue event result snapshot does not match event identity/version",
        ));
    }
    Ok(IssueEventV1 {
        id,
        project_id,
        issue_id,
        sequence: row.get(3)?,
        operation,
        actor_kind,
        actor_session_id,
        owning_epic_id,
        actor_label: row.get(8)?,
        expected_row_version: row.get(9)?,
        resulting_row_version,
        idempotency_key: row.get(11)?,
        request_fingerprint,
        occurred_at: occurred_at_value,
        request,
        issue,
    })
}

fn insert_issue_event_tx(tx: &Transaction<'_>, event: &IssueEventV1) -> Result<()> {
    validate_issue_event(event)?;
    let request_json = event
        .request
        .canonical_json()
        .map_err(DaemonError::InvalidParam)?;
    let result_json =
        canonical_program_run_json(&event.issue).map_err(DaemonError::InvalidParam)?;
    tx.execute(
        "INSERT INTO issue_events (
             id, project_id, issue_id, sequence, operation, actor_kind, actor_session_id,
             owning_epic_id, actor_label, expected_row_version, resulting_row_version,
             idempotency_key, request_fingerprint, request_json, result_json, occurred_at
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
         )",
        params![
            event.id.to_string(),
            event.project_id.to_string(),
            event.issue_id.to_string(),
            event.sequence,
            event.operation.as_str(),
            event.actor_kind.as_str(),
            event.actor_session_id.map(|id| id.to_string()),
            event.owning_epic_id.map(|id| id.to_string()),
            event.actor_label.as_deref(),
            event.expected_row_version,
            event.resulting_row_version,
            event.idempotency_key.as_deref(),
            &event.request_fingerprint,
            request_json,
            result_json,
            canonical_timestamp(event.occurred_at),
        ],
    )?;
    #[cfg(test)]
    issue_write_fault(IssueWriteFault::AfterEvent)?;
    Ok(())
}

fn validate_issue_event(event: &IssueEventV1) -> Result<()> {
    event
        .request
        .validate()
        .map_err(DaemonError::InvalidParam)?;
    let semantic_operation = event.request.operation.event_operation();
    if event.operation != semantic_operation
        && !(event.operation == IssueEventOperationV1::LegacyCreateAdopted
            && semantic_operation == IssueEventOperationV1::Created)
    {
        return Err(DaemonError::Store(
            "Issue event operation does not match semantic request".to_string(),
        ));
    }
    if event.issue.id != event.issue_id
        || event.issue.project_id != event.project_id
        || event.issue.row_version != event.resulting_row_version
        || event.resulting_row_version < 1
        || event.sequence != event.resulting_row_version
    {
        return Err(DaemonError::Store(
            "Issue event projection/version invariant failed".to_string(),
        ));
    }
    let lead_only_session_operation = matches!(
        event.operation,
        IssueEventOperationV1::ContentUpdated
            | IssueEventOperationV1::StatusUpdated
            | IssueEventOperationV1::Archived
            | IssueEventOperationV1::Restored
    );
    match event.actor_kind {
        IssueActorKindV1::Session => {
            if event.actor_session_id.is_none()
                || event.actor_label.is_some()
                || event.idempotency_key.is_none()
                || (lead_only_session_operation && event.owning_epic_id.is_none())
                || (!lead_only_session_operation && event.owning_epic_id.is_some())
            {
                return Err(DaemonError::Store(
                    "Issue session event provenance invariant failed".to_string(),
                ));
            }
        }
        IssueActorKindV1::Manager => {
            if event.actor_session_id.is_none()
                || event.actor_label.is_some()
                || event.idempotency_key.is_none()
                || event.owning_epic_id.is_some()
                || !lead_only_session_operation
            {
                return Err(DaemonError::Store(
                    "Issue manager event provenance invariant failed".to_string(),
                ));
            }
        }
        IssueActorKindV1::Operator | IssueActorKindV1::System => {
            if event.actor_session_id.is_some()
                || event.owning_epic_id.is_some()
                || event.idempotency_key.is_some()
                || event.actor_label.as_ref().is_none_or(|label| {
                    label.is_empty() || label.len() > 256 || label.contains('\0')
                })
            {
                return Err(DaemonError::Store(
                    "Issue operator/system event provenance invariant failed".to_string(),
                ));
            }
        }
    }
    let versions_valid = match event.operation {
        IssueEventOperationV1::BaselineImported | IssueEventOperationV1::Created => {
            event.expected_row_version == 0 && event.resulting_row_version == 1
        }
        IssueEventOperationV1::LegacyCreateAdopted => {
            event.expected_row_version == 1 && event.resulting_row_version == 2
        }
        _ => event.resulting_row_version == event.expected_row_version + 1,
    };
    if !versions_valid {
        return Err(DaemonError::Store(
            "Issue event expected/resulting version invariant failed".to_string(),
        ));
    }
    if event
        .request
        .fingerprint()
        .map_err(DaemonError::InvalidParam)?
        != event.request_fingerprint
        || canonical_timestamp(event.occurred_at).len() != 30
    {
        return Err(DaemonError::Store(
            "Issue event fingerprint/timestamp invariant failed".to_string(),
        ));
    }
    Ok(())
}

fn issue_create_semantic_request(id: Uuid, new: &NewIssue) -> IssueSemanticRequestV1 {
    IssueSemanticRequestV1::new(IssueSemanticOperationV1::Created {
        create: IssueCreateSemanticV1 {
            project_id: new.project_id,
            issue_id: id,
            title: new.title.clone(),
            body: new.body.clone(),
            priority: new.priority,
            labels: new.labels.clone(),
            created_by_session_id: new.created_by_session_id,
            assignee: new.assignee.clone(),
        },
    })
}

/// Append the V97 companion event for the existing atomic Idea-link writer.
/// The caller has already performed the link projection CAS in its own Idea
/// transaction; this helper keeps Issue version/audit advancement in that
/// same transaction instead of creating a second writer path.
pub(crate) fn append_issue_idea_link_event_tx(
    tx: &Transaction<'_>,
    issue: Issue,
    expected_row_version: i64,
    idea_id: Uuid,
    source_event_id: Option<Uuid>,
    source_finding_ref: Option<&IssueSourceFindingRef>,
) -> Result<()> {
    let event = build_issue_event(
        issue.clone(),
        issue.row_version,
        IssueEventOperationV1::IdeaLinked,
        &IssueWriteActor::Operator {
            label: "rsi-rpc:LinkIssueToIdea".to_string(),
        },
        expected_row_version,
        IssueSemanticRequestV1::new(IssueSemanticOperationV1::IdeaLinked {
            issue_id: issue.id,
            expected_row_version,
            idea_id,
            source_event_id,
            source_finding_ref: source_finding_ref.cloned(),
        }),
        issue.updated_at,
    )?;
    insert_issue_event_tx(tx, &event)
}

fn build_issue_event(
    issue: Issue,
    sequence: i64,
    operation: IssueEventOperationV1,
    actor: &IssueWriteActor,
    expected_row_version: i64,
    request: IssueSemanticRequestV1,
    occurred_at: chrono::DateTime<Utc>,
) -> Result<IssueEventV1> {
    let request_fingerprint = request.fingerprint().map_err(DaemonError::InvalidParam)?;
    let (actor_kind, actor_session_id, owning_epic_id, actor_label, idempotency_key) =
        actor.event_fields();
    Ok(IssueEventV1 {
        id: issue_event_id(issue.id, actor),
        project_id: issue.project_id,
        issue_id: issue.id,
        sequence,
        operation,
        actor_kind,
        actor_session_id,
        owning_epic_id,
        actor_label,
        expected_row_version,
        resulting_row_version: issue.row_version,
        idempotency_key,
        request_fingerprint,
        occurred_at,
        request,
        issue,
    })
}

#[derive(Debug)]
pub(crate) struct IssueV96MigrationWitness {
    retained_catalog: Vec<(String, String, String, String)>,
    issue_projection: Vec<String>,
    dependencies: Vec<String>,
}

const V94_ISSUES_TABLE_SQL: &str = r#"CREATE TABLE "issues" (
    id TEXT PRIMARY KEY CHECK(length(id) = 36 AND id = lower(id)),
    project_id TEXT NOT NULL CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
    display_number INTEGER NOT NULL UNIQUE,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'Open'
        CHECK(status IN ('Open','InProgress','Closed','Cancelled')),
    priority INTEGER CHECK(priority IS NULL OR priority BETWEEN 1 AND 4),
    labels TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(labels)),
    created_by_session_id TEXT,
    assignee TEXT,
    idea_id TEXT CHECK(idea_id IS NULL OR (length(idea_id) = 36 AND idea_id = lower(idea_id))),
    source_event_id TEXT CHECK(source_event_id IS NULL OR (length(source_event_id) = 36 AND source_event_id = lower(source_event_id))),
    source_finding_ref TEXT CHECK(source_finding_ref IS NULL OR (
        length(CAST(source_finding_ref AS BLOB)) BETWEEN 84 AND 211
        AND substr(source_finding_ref, 1, 18) = 'finding:v1:sha256:'
        AND length(substr(source_finding_ref, 19, 64)) = 64
        AND substr(source_finding_ref, 19, 64) NOT GLOB '*[^0-9a-f]*'
        AND substr(source_finding_ref, 83, 1) = ':'
        AND length(substr(source_finding_ref, 84)) BETWEEN 1 AND 128
        AND substr(source_finding_ref, 84) NOT GLOB '*[^A-Za-z0-9._-]*'
    )),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    closed_at TEXT,
    UNIQUE(id, project_id),
    CHECK(source_event_id IS NULL OR idea_id IS NOT NULL),
    CHECK(source_finding_ref IS NULL OR idea_id IS NOT NULL),
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE RESTRICT,
    FOREIGN KEY(idea_id, project_id) REFERENCES ideas(id, project_id) ON DELETE RESTRICT,
    FOREIGN KEY(source_event_id, idea_id, project_id)
        REFERENCES idea_events(id, idea_id, project_id) ON DELETE RESTRICT
)"#;

const V94_ISSUE_DEPS_TABLE_SQL: &str = r#"CREATE TABLE "issue_deps" (
    project_id TEXT NOT NULL CHECK(length(project_id) = 36 AND project_id = lower(project_id)),
    issue_id TEXT NOT NULL,
    depends_on_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(project_id, issue_id, depends_on_id),
    CHECK(issue_id <> depends_on_id),
    FOREIGN KEY(issue_id, project_id) REFERENCES "issues"(id, project_id) ON DELETE CASCADE,
    FOREIGN KEY(depends_on_id, project_id) REFERENCES "issues"(id, project_id) ON DELETE CASCADE
)"#;

const V97_ROW_VERSION_COLUMN_SQL: &str =
    "row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version >= 1)";
const V97_ARCHIVED_AT_COLUMN_SQL: &str = "archived_at TEXT CHECK(archived_at IS NULL OR (length(archived_at)=30 AND archived_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]Z'))";

#[derive(Clone, Copy)]
struct ColumnCatalogSpec {
    name: &'static str,
    declared_type: &'static str,
    not_null: i64,
    default_value: Option<&'static str>,
    primary_key_position: i64,
    hidden: i64,
}

const V94_ISSUE_COLUMN_CATALOG: &[ColumnCatalogSpec] = &[
    ColumnCatalogSpec {
        name: "id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 1,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "project_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "display_number",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "title",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "body",
        declared_type: "TEXT",
        not_null: 1,
        default_value: Some("''"),
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "status",
        declared_type: "TEXT",
        not_null: 1,
        default_value: Some("'Open'"),
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "priority",
        declared_type: "INTEGER",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "labels",
        declared_type: "TEXT",
        not_null: 1,
        default_value: Some("'[]'"),
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "created_by_session_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "assignee",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "idea_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "source_event_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "source_finding_ref",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "created_at",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "updated_at",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "closed_at",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
];

const V97_ISSUE_COLUMN_CATALOG: &[ColumnCatalogSpec] = &[
    V94_ISSUE_COLUMN_CATALOG[0],
    V94_ISSUE_COLUMN_CATALOG[1],
    V94_ISSUE_COLUMN_CATALOG[2],
    V94_ISSUE_COLUMN_CATALOG[3],
    V94_ISSUE_COLUMN_CATALOG[4],
    V94_ISSUE_COLUMN_CATALOG[5],
    V94_ISSUE_COLUMN_CATALOG[6],
    V94_ISSUE_COLUMN_CATALOG[7],
    V94_ISSUE_COLUMN_CATALOG[8],
    V94_ISSUE_COLUMN_CATALOG[9],
    V94_ISSUE_COLUMN_CATALOG[10],
    V94_ISSUE_COLUMN_CATALOG[11],
    V94_ISSUE_COLUMN_CATALOG[12],
    V94_ISSUE_COLUMN_CATALOG[13],
    V94_ISSUE_COLUMN_CATALOG[14],
    V94_ISSUE_COLUMN_CATALOG[15],
    ColumnCatalogSpec {
        name: "row_version",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: Some("1"),
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "archived_at",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
];

const ISSUE_DEPS_COLUMN_CATALOG: &[ColumnCatalogSpec] = &[
    ColumnCatalogSpec {
        name: "project_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 1,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "issue_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 2,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "depends_on_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 3,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "created_at",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
];

const ISSUE_EVENTS_COLUMN_CATALOG: &[ColumnCatalogSpec] = &[
    ColumnCatalogSpec {
        name: "id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 1,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "project_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "issue_id",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "sequence",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "operation",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "actor_kind",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "actor_session_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "owning_epic_id",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "actor_label",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "expected_row_version",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "resulting_row_version",
        declared_type: "INTEGER",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "idempotency_key",
        declared_type: "TEXT",
        not_null: 0,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "request_fingerprint",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "request_json",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "result_json",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
    ColumnCatalogSpec {
        name: "occurred_at",
        declared_type: "TEXT",
        not_null: 1,
        default_value: None,
        primary_key_position: 0,
        hidden: 0,
    },
];

#[derive(Clone, Copy)]
struct IndexCatalogSpec {
    name: &'static str,
    unique: i64,
    origin: &'static str,
    partial: i64,
}

const ISSUES_INDEX_CATALOG: &[IndexCatalogSpec] = &[
    IndexCatalogSpec {
        name: "idx_issues_created_by",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "idx_issues_project_creator",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "idx_issues_project_display",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "idx_issues_project_idea",
        unique: 0,
        origin: "c",
        partial: 1,
    },
    IndexCatalogSpec {
        name: "idx_issues_project_ready",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "idx_issues_source_event",
        unique: 0,
        origin: "c",
        partial: 1,
    },
    IndexCatalogSpec {
        name: "idx_issues_source_finding",
        unique: 0,
        origin: "c",
        partial: 1,
    },
    IndexCatalogSpec {
        name: "idx_issues_status",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issues_1",
        unique: 1,
        origin: "pk",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issues_2",
        unique: 1,
        origin: "u",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issues_3",
        unique: 1,
        origin: "u",
        partial: 0,
    },
];

const ISSUE_DEPS_INDEX_CATALOG: &[IndexCatalogSpec] = &[
    IndexCatalogSpec {
        name: "idx_issue_deps_blocker",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issue_deps_1",
        unique: 1,
        origin: "pk",
        partial: 0,
    },
];

const ISSUE_EVENTS_INDEX_CATALOG: &[IndexCatalogSpec] = &[
    IndexCatalogSpec {
        name: "idx_issue_events_actor_key",
        unique: 1,
        origin: "c",
        partial: 1,
    },
    IndexCatalogSpec {
        name: "idx_issue_events_issue_sequence",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "idx_issue_events_project_time",
        unique: 0,
        origin: "c",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issue_events_1",
        unique: 1,
        origin: "pk",
        partial: 0,
    },
    IndexCatalogSpec {
        name: "sqlite_autoindex_issue_events_2",
        unique: 1,
        origin: "u",
        partial: 0,
    },
];

#[derive(Clone, Copy)]
struct ForeignKeyCatalogSpec {
    id: i64,
    sequence: i64,
    target_table: &'static str,
    source_column: &'static str,
    target_column: &'static str,
    on_update: &'static str,
    on_delete: &'static str,
    match_kind: &'static str,
}

const ISSUES_FOREIGN_KEY_CATALOG: &[ForeignKeyCatalogSpec] = &[
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 0,
        target_table: "idea_events",
        source_column: "source_event_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 1,
        target_table: "idea_events",
        source_column: "idea_id",
        target_column: "idea_id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 2,
        target_table: "idea_events",
        source_column: "project_id",
        target_column: "project_id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 1,
        sequence: 0,
        target_table: "ideas",
        source_column: "idea_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 1,
        sequence: 1,
        target_table: "ideas",
        source_column: "project_id",
        target_column: "project_id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 2,
        sequence: 0,
        target_table: "projects",
        source_column: "project_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
];

const ISSUE_DEPS_FOREIGN_KEY_CATALOG: &[ForeignKeyCatalogSpec] = &[
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 0,
        target_table: "issues",
        source_column: "depends_on_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "CASCADE",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 1,
        target_table: "issues",
        source_column: "project_id",
        target_column: "project_id",
        on_update: "NO ACTION",
        on_delete: "CASCADE",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 1,
        sequence: 0,
        target_table: "issues",
        source_column: "issue_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "CASCADE",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 1,
        sequence: 1,
        target_table: "issues",
        source_column: "project_id",
        target_column: "project_id",
        on_update: "NO ACTION",
        on_delete: "CASCADE",
        match_kind: "NONE",
    },
];

const ISSUE_EVENTS_FOREIGN_KEY_CATALOG: &[ForeignKeyCatalogSpec] = &[
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 0,
        target_table: "issues",
        source_column: "issue_id",
        target_column: "id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
    ForeignKeyCatalogSpec {
        id: 0,
        sequence: 1,
        target_table: "issues",
        source_column: "project_id",
        target_column: "project_id",
        on_update: "NO ACTION",
        on_delete: "RESTRICT",
        match_kind: "NONE",
    },
];

const V94_ISSUE_INDEX_SQL: &[(&str, &str)] = &[
    (
        "idx_issues_status",
        "CREATE INDEX idx_issues_status ON issues(status, display_number)",
    ),
    (
        "idx_issues_created_by",
        "CREATE INDEX idx_issues_created_by ON issues(created_by_session_id, display_number)",
    ),
    (
        "idx_issues_project_display",
        "CREATE INDEX idx_issues_project_display ON issues(project_id, display_number)",
    ),
    (
        "idx_issues_project_ready",
        "CREATE INDEX idx_issues_project_ready ON issues(project_id, status, (priority IS NULL), priority, created_at, id)",
    ),
    (
        "idx_issues_project_creator",
        "CREATE INDEX idx_issues_project_creator ON issues(project_id, created_by_session_id, display_number)",
    ),
    (
        "idx_issues_project_idea",
        "CREATE INDEX idx_issues_project_idea ON issues(project_id, idea_id, display_number) WHERE idea_id IS NOT NULL",
    ),
    (
        "idx_issues_source_event",
        "CREATE INDEX idx_issues_source_event ON issues(project_id, idea_id, source_event_id) WHERE source_event_id IS NOT NULL",
    ),
    (
        "idx_issues_source_finding",
        "CREATE INDEX idx_issues_source_finding ON issues(project_id, source_finding_ref) WHERE source_finding_ref IS NOT NULL",
    ),
    (
        "idx_issue_deps_blocker",
        "CREATE INDEX idx_issue_deps_blocker ON issue_deps(project_id, depends_on_id, issue_id)",
    ),
];

const V97_READY_INDEX_SQL: &str = "CREATE INDEX idx_issues_project_ready ON issues(project_id, status, archived_at, (priority IS NULL), priority, created_at, id)";

const V97_EVENT_TABLE_SQL: &str =
    "CREATE TABLE issue_events (
             id TEXT PRIMARY KEY CHECK(length(id)=36 AND id=lower(id)),
             project_id TEXT NOT NULL CHECK(length(project_id)=36 AND project_id=lower(project_id)),
             issue_id TEXT NOT NULL CHECK(length(issue_id)=36 AND issue_id=lower(issue_id)),
             sequence INTEGER NOT NULL CHECK(sequence>=1),
             operation TEXT NOT NULL CHECK(operation IN ('baseline_imported','created','content_updated','status_updated','archived','restored','idea_linked','legacy_create_adopted')),
             actor_kind TEXT NOT NULL CHECK(actor_kind IN ('operator','session','system')),
             actor_session_id TEXT CHECK(actor_session_id IS NULL OR (length(actor_session_id)=36 AND actor_session_id=lower(actor_session_id))),
             owning_epic_id TEXT CHECK(owning_epic_id IS NULL OR (length(owning_epic_id)=36 AND owning_epic_id=lower(owning_epic_id))),
             actor_label TEXT CHECK(actor_label IS NULL OR (length(CAST(actor_label AS BLOB)) BETWEEN 1 AND 256 AND instr(actor_label,char(0))=0)),
             expected_row_version INTEGER NOT NULL CHECK(expected_row_version>=0),
             resulting_row_version INTEGER NOT NULL CHECK(resulting_row_version>=1),
             idempotency_key TEXT CHECK(idempotency_key IS NULL OR (length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 128 AND instr(idempotency_key,char(0))=0)),
             request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
             request_json TEXT NOT NULL CHECK(json_valid(request_json) AND json_extract(request_json,'$.domain')='rsi.issue.request/v1'),
             result_json TEXT NOT NULL CHECK(json_valid(result_json) AND json_extract(result_json,'$.id')=issue_id AND json_extract(result_json,'$.project_id')=project_id AND json_extract(result_json,'$.row_version')=resulting_row_version),
             occurred_at TEXT NOT NULL CHECK(length(occurred_at)=30 AND occurred_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]Z'),
             UNIQUE(issue_id,sequence),
             FOREIGN KEY(issue_id,project_id) REFERENCES issues(id,project_id) ON DELETE RESTRICT,
             CHECK(
                 (actor_kind='session' AND actor_session_id IS NOT NULL AND actor_label IS NULL AND idempotency_key IS NOT NULL)
                 OR (actor_kind IN ('operator','system') AND actor_session_id IS NULL AND owning_epic_id IS NULL AND actor_label IS NOT NULL AND idempotency_key IS NULL)
             ),
             CHECK(owning_epic_id IS NULL OR actor_kind='session'),
             CHECK(
                 actor_kind!='session'
                 OR operation IN ('created','legacy_create_adopted')
                 OR owning_epic_id IS NOT NULL
             ),
             CHECK(
                 actor_kind!='session'
                 OR operation NOT IN ('created','legacy_create_adopted')
                 OR owning_epic_id IS NULL
             ),
             CHECK(
                 (operation IN ('baseline_imported','created') AND expected_row_version=0 AND resulting_row_version=1)
                 OR (operation='legacy_create_adopted' AND expected_row_version=1 AND resulting_row_version=2)
                 OR (operation NOT IN ('baseline_imported','created','legacy_create_adopted') AND resulting_row_version=expected_row_version+1)
             ),
             CHECK(sequence=resulting_row_version),
             CHECK(
                 json_extract(request_json,'$.operation')=operation
                 OR (operation='legacy_create_adopted' AND json_extract(request_json,'$.operation')='created')
             )
         )";

const V97_INDEX_SQL: &[(&str, &str)] = &[
    (
        "idx_issue_events_actor_key",
        "CREATE UNIQUE INDEX idx_issue_events_actor_key ON issue_events(actor_session_id,idempotency_key) WHERE actor_session_id IS NOT NULL AND idempotency_key IS NOT NULL",
    ),
    (
        "idx_issue_events_project_time",
        "CREATE INDEX idx_issue_events_project_time ON issue_events(project_id,occurred_at,id)",
    ),
    (
        "idx_issue_events_issue_sequence",
        "CREATE INDEX idx_issue_events_issue_sequence ON issue_events(issue_id,sequence)",
    ),
];

const V97_TRIGGER_SQL: &[(&str, &str)] = &[
    (
        "issue_events_no_update",
        "CREATE TRIGGER issue_events_no_update BEFORE UPDATE ON issue_events BEGIN SELECT RAISE(ABORT,'issue_events_are_immutable'); END",
    ),
    (
        "issue_events_no_delete",
        "CREATE TRIGGER issue_events_no_delete BEFORE DELETE ON issue_events BEGIN SELECT RAISE(ABORT,'issue_events_are_immutable'); END",
    ),
    (
        "issues_no_delete",
        "CREATE TRIGGER issues_no_delete BEFORE DELETE ON issues BEGIN SELECT RAISE(ABORT,'issues_are_append_only'); END",
    ),
];

fn normalized_sql(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn expected_v97_issues_table_sql() -> Result<String> {
    let v94 = normalized_sql(V94_ISSUES_TABLE_SQL);
    let marker = "closed_at TEXT, UNIQUE(id, project_id),";
    if v94.matches(marker).count() != 1 {
        return Err(DaemonError::Store(
            "static V94 Issue catalog cannot derive V97 table SQL".to_string(),
        ));
    }
    Ok(v94.replacen(
        marker,
        &format!(
            "closed_at TEXT, {V97_ROW_VERSION_COLUMN_SQL}, {V97_ARCHIVED_AT_COLUMN_SQL}, UNIQUE(id, project_id),"
        ),
        1,
    ))
}

fn require_catalog_object_sql(
    tx: &Transaction<'_>,
    object_type: &str,
    name: &str,
    table_name: &str,
    expected: &str,
) -> Result<()> {
    let actual: Option<(String, String, String)> = tx
        .query_row(
            "SELECT type,tbl_name,COALESCE(sql,'') FROM sqlite_master WHERE name=?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let expected_sql = normalized_sql(expected);
    if actual
        .as_ref()
        .map(|(kind, table, sql)| (kind.as_str(), table.as_str(), normalized_sql(sql)))
        != Some((object_type, table_name, expected_sql))
    {
        return Err(DaemonError::Store(format!(
            "Issue catalog SQL mismatch for {name}"
        )));
    }
    Ok(())
}

fn require_issue_catalog_object_count(tx: &Transaction<'_>, expected: i64) -> Result<()> {
    let actual: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_master
         WHERE tbl_name IN ('issues','issue_deps','issue_events') AND sql IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if actual != expected {
        return Err(DaemonError::Store(format!(
            "Issue catalog object membership mismatch: expected {expected}, found {actual}"
        )));
    }
    Ok(())
}

fn require_column_catalog(
    tx: &Transaction<'_>,
    table: &str,
    expected: &[ColumnCatalogSpec],
) -> Result<()> {
    let actual = tx
        .prepare(
            "SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden
             FROM pragma_table_xinfo(?1) ORDER BY cid",
        )?
        .query_map([table], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let matches = actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .enumerate()
            .all(|(cid, (actual, expected))| {
                actual.0 == cid as i64
                    && actual.1 == expected.name
                    && actual.2 == expected.declared_type
                    && actual.3 == expected.not_null
                    && actual.4.as_deref() == expected.default_value
                    && actual.5 == expected.primary_key_position
                    && actual.6 == expected.hidden
            });
    if !matches {
        return Err(DaemonError::Store(format!(
            "Issue catalog column metadata mismatch for {table}"
        )));
    }
    Ok(())
}

fn require_index_catalog(
    tx: &Transaction<'_>,
    table: &str,
    expected: &[IndexCatalogSpec],
) -> Result<()> {
    let actual = tx
        .prepare("SELECT name,\"unique\",origin,partial FROM pragma_index_list(?1) ORDER BY name")?
        .query_map([table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let matches = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual.0 == expected.name
                && actual.1 == expected.unique
                && actual.2 == expected.origin
                && actual.3 == expected.partial
        });
    if !matches {
        return Err(DaemonError::Store(format!(
            "Issue catalog index metadata mismatch for {table}"
        )));
    }
    Ok(())
}

fn require_foreign_key_catalog(
    tx: &Transaction<'_>,
    table: &str,
    expected: &[ForeignKeyCatalogSpec],
) -> Result<()> {
    let actual = tx
        .prepare(
            "SELECT id,seq,\"table\",\"from\",\"to\",on_update,on_delete,match
             FROM pragma_foreign_key_list(?1) ORDER BY id,seq",
        )?
        .query_map([table], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let matches = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual.0 == expected.id
                && actual.1 == expected.sequence
                && actual.2 == expected.target_table
                && actual.3 == expected.source_column
                && actual.4 == expected.target_column
                && actual.5 == expected.on_update
                && actual.6 == expected.on_delete
                && actual.7 == expected.match_kind
        });
    if !matches {
        return Err(DaemonError::Store(format!(
            "Issue catalog foreign-key metadata mismatch for {table}"
        )));
    }
    Ok(())
}

fn require_v94_issue_projection_catalog(tx: &Transaction<'_>) -> Result<()> {
    require_issue_catalog_object_count(tx, 11)?;
    require_catalog_object_sql(tx, "table", "issues", "issues", V94_ISSUES_TABLE_SQL)?;
    require_catalog_object_sql(
        tx,
        "table",
        "issue_deps",
        "issue_deps",
        V94_ISSUE_DEPS_TABLE_SQL,
    )?;
    require_column_catalog(tx, "issues", V94_ISSUE_COLUMN_CATALOG)?;
    require_column_catalog(tx, "issue_deps", ISSUE_DEPS_COLUMN_CATALOG)?;
    require_index_catalog(tx, "issues", ISSUES_INDEX_CATALOG)?;
    require_index_catalog(tx, "issue_deps", ISSUE_DEPS_INDEX_CATALOG)?;
    require_foreign_key_catalog(tx, "issues", ISSUES_FOREIGN_KEY_CATALOG)?;
    require_foreign_key_catalog(tx, "issue_deps", ISSUE_DEPS_FOREIGN_KEY_CATALOG)?;
    for (name, sql) in V94_ISSUE_INDEX_SQL {
        let table = if *name == "idx_issue_deps_blocker" {
            "issue_deps"
        } else {
            "issues"
        };
        require_catalog_object_sql(tx, "index", name, table, sql)?;
    }
    Ok(())
}

fn issue_projection_snapshot(tx: &Transaction<'_>) -> Result<Vec<String>> {
    Ok(tx
        .prepare(
            "SELECT json_array(id,project_id,display_number,title,body,status,priority,labels,
                               created_by_session_id,assignee,idea_id,source_event_id,
                               source_finding_ref,created_at,updated_at,closed_at)
             FROM issues ORDER BY display_number,id",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn issue_dependency_snapshot(tx: &Transaction<'_>) -> Result<Vec<String>> {
    Ok(tx
        .prepare(
            "SELECT json_array(project_id,issue_id,depends_on_id,created_at)
             FROM issue_deps ORDER BY project_id,issue_id,depends_on_id",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn retained_catalog_snapshot(
    tx: &Transaction<'_>,
    v97: bool,
) -> Result<Vec<(String, String, String, String)>> {
    if v97 {
        return Ok(tx
            .prepare(
                "SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%'
                   AND name NOT IN (
                     'issues','idx_issues_project_ready','issue_events',
                     'idx_issue_events_actor_key','idx_issue_events_project_time',
                     'idx_issue_events_issue_sequence','issue_events_no_update',
                     'issue_events_no_delete','issues_no_delete'
                   )
                 ORDER BY type,name",
            )?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?);
    }
    Ok(tx
        .prepare(
            "SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%'
               AND name NOT IN ('issues','idx_issues_project_ready')
             ORDER BY type,name",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn validate_v96_issue_source_catalog(
    tx: &Transaction<'_>,
) -> Result<IssueV96MigrationWitness> {
    super::cohort_settlement::validate_v95_catalog(tx)?;
    require_v94_issue_projection_catalog(tx)?;
    let foreign_key_errors: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(DaemonError::Store(
            "V96 Issue source has foreign-key violations".to_string(),
        ));
    }
    Ok(IssueV96MigrationWitness {
        retained_catalog: retained_catalog_snapshot(tx, false)?,
        issue_projection: issue_projection_snapshot(tx)?,
        dependencies: issue_dependency_snapshot(tx)?,
    })
}

pub(crate) fn add_v97_issue_columns(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "ALTER TABLE issues ADD COLUMN row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version >= 1)",
        [],
    )?;
    tx.execute(
        "ALTER TABLE issues ADD COLUMN archived_at TEXT CHECK(archived_at IS NULL OR (length(archived_at)=30 AND archived_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]Z'))",
        [],
    )?;
    Ok(())
}

pub(crate) fn create_v97_issue_event_table(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(V97_EVENT_TABLE_SQL)?;
    Ok(())
}

pub(crate) fn create_v97_issue_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute("DROP INDEX idx_issues_project_ready", [])?;
    tx.execute(V97_READY_INDEX_SQL, [])?;
    tx.execute(
        "CREATE UNIQUE INDEX idx_issue_events_actor_key ON issue_events(actor_session_id,idempotency_key) WHERE actor_session_id IS NOT NULL AND idempotency_key IS NOT NULL",
        [],
    )?;
    tx.execute(
        "CREATE INDEX idx_issue_events_project_time ON issue_events(project_id,occurred_at,id)",
        [],
    )?;
    tx.execute(
        "CREATE INDEX idx_issue_events_issue_sequence ON issue_events(issue_id,sequence)",
        [],
    )?;
    Ok(())
}

pub(crate) fn create_v97_issue_triggers(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TRIGGER issue_events_no_update BEFORE UPDATE ON issue_events BEGIN SELECT RAISE(ABORT,'issue_events_are_immutable'); END",
    )?;
    tx.execute_batch(
        "CREATE TRIGGER issue_events_no_delete BEFORE DELETE ON issue_events BEGIN SELECT RAISE(ABORT,'issue_events_are_immutable'); END",
    )?;
    tx.execute_batch(
        "CREATE TRIGGER issues_no_delete BEFORE DELETE ON issues BEGIN SELECT RAISE(ABORT,'issues_are_append_only'); END",
    )?;
    Ok(())
}

pub(crate) fn backfill_v97_issue_events(tx: &Transaction<'_>) -> Result<()> {
    let rows = tx
        .prepare(&format!(
            "SELECT {ISSUE_COLUMNS} FROM issues ORDER BY display_number,id"
        ))?
        .query_map([], map_issue_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        let issue = row.into_issue()?;
        if issue.row_version != 1 || issue.archived_at.is_some() {
            return Err(DaemonError::Store(
                "V97 Issue baseline source has non-initial version/archive state".to_string(),
            ));
        }
        let request = IssueSemanticRequestV1::new(IssueSemanticOperationV1::BaselineImported {
            issue_id: issue.id,
            project_id: issue.project_id,
        });
        let event = IssueEventV1 {
            id: baseline_issue_event_id(issue.id),
            project_id: issue.project_id,
            issue_id: issue.id,
            sequence: 1,
            operation: IssueEventOperationV1::BaselineImported,
            actor_kind: IssueActorKindV1::System,
            actor_session_id: None,
            owning_epic_id: None,
            actor_label: Some("rsi:v97-baseline-import".to_string()),
            expected_row_version: 0,
            resulting_row_version: 1,
            idempotency_key: None,
            request_fingerprint: request.fingerprint().map_err(DaemonError::InvalidParam)?,
            occurred_at: issue.updated_at,
            request,
            issue,
        };
        insert_issue_event_tx(tx, &event)?;
    }
    Ok(())
}

pub(crate) fn validate_v97_catalog(
    tx: &Transaction<'_>,
    witness: &IssueV96MigrationWitness,
) -> Result<()> {
    require_issue_catalog_object_count(tx, 18)?;
    let issues_table_sql = expected_v97_issues_table_sql()?;
    require_catalog_object_sql(tx, "table", "issues", "issues", &issues_table_sql)?;
    require_catalog_object_sql(
        tx,
        "table",
        "issue_deps",
        "issue_deps",
        V94_ISSUE_DEPS_TABLE_SQL,
    )?;
    require_catalog_object_sql(
        tx,
        "table",
        "issue_events",
        "issue_events",
        V97_EVENT_TABLE_SQL,
    )?;
    require_column_catalog(tx, "issues", V97_ISSUE_COLUMN_CATALOG)?;
    require_column_catalog(tx, "issue_deps", ISSUE_DEPS_COLUMN_CATALOG)?;
    require_column_catalog(tx, "issue_events", ISSUE_EVENTS_COLUMN_CATALOG)?;
    require_index_catalog(tx, "issues", ISSUES_INDEX_CATALOG)?;
    require_index_catalog(tx, "issue_deps", ISSUE_DEPS_INDEX_CATALOG)?;
    require_index_catalog(tx, "issue_events", ISSUE_EVENTS_INDEX_CATALOG)?;
    require_foreign_key_catalog(tx, "issues", ISSUES_FOREIGN_KEY_CATALOG)?;
    require_foreign_key_catalog(tx, "issue_deps", ISSUE_DEPS_FOREIGN_KEY_CATALOG)?;
    require_foreign_key_catalog(tx, "issue_events", ISSUE_EVENTS_FOREIGN_KEY_CATALOG)?;
    for (name, sql) in V94_ISSUE_INDEX_SQL {
        let (table, expected_sql) = if *name == "idx_issue_deps_blocker" {
            ("issue_deps", *sql)
        } else if *name == "idx_issues_project_ready" {
            ("issues", V97_READY_INDEX_SQL)
        } else {
            ("issues", *sql)
        };
        require_catalog_object_sql(tx, "index", name, table, expected_sql)?;
    }
    for (name, sql) in V97_INDEX_SQL.iter().chain(V97_TRIGGER_SQL) {
        let (object_type, table) = if name.starts_with("idx_") {
            ("index", "issue_events")
        } else if *name == "issues_no_delete" {
            ("trigger", "issues")
        } else {
            ("trigger", "issue_events")
        };
        require_catalog_object_sql(tx, object_type, name, table, sql)?;
    }
    if retained_catalog_snapshot(tx, true)? != witness.retained_catalog {
        return Err(DaemonError::Store(
            "V97 migration changed retained V94 catalog objects".to_string(),
        ));
    }
    if issue_projection_snapshot(tx)? != witness.issue_projection
        || issue_dependency_snapshot(tx)? != witness.dependencies
    {
        return Err(DaemonError::Store(
            "V97 Issue/dependency projection parity failed".to_string(),
        ));
    }
    let issue_count: i64 = tx.query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?;
    let event_count: i64 = tx.query_row(
        "SELECT count(*) FROM issue_events WHERE operation='baseline_imported'",
        [],
        |row| row.get(0),
    )?;
    if issue_count != event_count {
        return Err(DaemonError::Store(
            "V97 Issue baseline backfill parity failed".to_string(),
        ));
    }
    let invalid_projection: i64 = tx.query_row(
        "SELECT count(*) FROM issues WHERE row_version!=1 OR archived_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if invalid_projection != 0 {
        return Err(DaemonError::Store(
            "V97 imported Issue projection initialization mismatch".to_string(),
        ));
    }
    let events = tx
        .prepare(&format!(
            "SELECT {ISSUE_EVENT_COLUMNS} FROM issue_events ORDER BY issue_id,sequence"
        ))?
        .query_map([], issue_event_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if events.iter().any(|event| {
        event.operation != IssueEventOperationV1::BaselineImported
            || event.id != baseline_issue_event_id(event.issue_id)
    }) {
        return Err(DaemonError::Store(
            "V97 deterministic baseline event validation failed".to_string(),
        ));
    }
    Ok(())
}

impl Store {
    pub(super) fn get_issue_tx(tx: &rusqlite::Transaction<'_>, id: Uuid) -> Result<Option<Issue>> {
        let mut stmt = tx.prepare(&format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE id = ?1"))?;
        let mut rows = stmt.query(params![id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_issue_row(row)?.into_issue()?)),
            None => Ok(None),
        }
    }

    fn validate_unlinked_new_issue_tx(
        tx: &rusqlite::Transaction<'_>,
        new: &NewIssue,
    ) -> Result<()> {
        if new.project_id.is_nil() {
            return Err(DaemonError::Store(
                "Issue project_id must not be nil".to_string(),
            ));
        }
        let project_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
            params![new.project_id.to_string()],
            |row| row.get(0),
        )?;
        if !project_exists {
            return Err(DaemonError::Store(
                "Issue project does not exist".to_string(),
            ));
        }
        if new.idea_id.is_some()
            || new.source_event_id.is_some()
            || new.source_finding_ref.is_some()
        {
            return Err(DaemonError::Store(
                "Issues must be created unlinked".to_string(),
            ));
        }
        Ok(())
    }

    pub(super) fn idempotent_issue_in_tx(
        tx: &rusqlite::Transaction<'_>,
        id: Uuid,
        new: &NewIssue,
        actor: IssueWriteActor,
    ) -> Result<IdempotentIssueCreate> {
        let existing = Self::get_issue_tx(tx, id)?;
        if let Some(issue) = existing {
            // V97 agent create replay is receipt-backed rather than
            // projection-backed. Later content/status changes must not make an
            // exact original retry look changed or return a reconstructed
            // current Issue. Pre-V97 deterministic rows have no receipt and
            // deliberately retain the legacy comparison path until adoption.
            let receipt = match &actor {
                IssueWriteActor::Session {
                    session_id,
                    idempotency_key,
                    ..
                } => Self::event_by_actor_key_tx(tx, *session_id, idempotency_key)?
                    .filter(|event| {
                        event.issue_id == id
                            && matches!(
                                event.operation,
                                IssueEventOperationV1::Created
                                    | IssueEventOperationV1::LegacyCreateAdopted
                            )
                    })
                    .map(|event| event.issue),
                IssueWriteActor::OperatorIdempotent { event_id, .. } => {
                    Self::event_by_id_tx(tx, *event_id)?
                        .filter(|event| {
                            event.issue_id == id
                                && matches!(
                                    event.operation,
                                    IssueEventOperationV1::Created
                                        | IssueEventOperationV1::LegacyCreateAdopted
                                )
                        })
                        .map(|event| event.issue)
                }
                _ => None,
            };
            let replay_issue = receipt.clone().unwrap_or_else(|| issue.clone());
            let create_fields_match = replay_issue.title == new.title
                && replay_issue.project_id == new.project_id
                && replay_issue.body == new.body
                && replay_issue.priority == new.priority
                && replay_issue.labels == new.labels
                && replay_issue.created_by_session_id == new.created_by_session_id
                && replay_issue.assignee == new.assignee
                && replay_issue.idea_id == new.idea_id
                && replay_issue.source_event_id == new.source_event_id
                && replay_issue.source_finding_ref == new.source_finding_ref;
            if receipt.is_none()
                && create_fields_match
                && let IssueWriteActor::Session { .. } = &actor
            {
                if issue.row_version != 1 {
                    return Ok(IdempotentIssueCreate {
                        issue,
                        deduplicated: true,
                        create_fields_match: false,
                    });
                }
                // The one-time V97 adoption receipt makes later exact create
                // retries independent of mutable projection state.
                let changed = tx.execute(
                    "UPDATE issues SET row_version=row_version+1 WHERE id=?1 AND project_id=?2 AND row_version=1",
                    params![issue.id.to_string(), issue.project_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(DaemonError::Store(
                        "legacy Issue create adoption lost CAS".into(),
                    ));
                }
                issue_projection_written()?;
                let mut adopted = issue.clone();
                adopted.row_version = 2;
                let event = build_issue_event(
                    adopted.clone(),
                    2,
                    IssueEventOperationV1::LegacyCreateAdopted,
                    &actor,
                    1,
                    issue_create_semantic_request(issue.id, new),
                    issue.updated_at,
                )?;
                insert_issue_event_tx(tx, &event)?;
                return Ok(IdempotentIssueCreate {
                    issue: adopted,
                    deduplicated: true,
                    create_fields_match: true,
                });
            }
            return Ok(IdempotentIssueCreate {
                issue: replay_issue,
                deduplicated: true,
                create_fields_match,
            });
        }

        Self::validate_unlinked_new_issue_tx(tx, new)?;
        let now = now_nanos();
        let labels_json = serde_json::to_string(&new.labels)?;
        let display_number: i64 = tx.query_row(
            "SELECT COALESCE(MAX(display_number), 0) + 1 FROM issues",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO issues (id, project_id, display_number, title, body, status, priority, labels,
                                 created_by_session_id, assignee, idea_id, source_event_id, source_finding_ref,
                                 created_at, updated_at, closed_at, row_version, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL, NULL, ?11, ?12, NULL, 1, NULL)",
            params![
                id.to_string(),
                new.project_id.to_string(),
                display_number,
                new.title,
                new.body,
                IssueStatus::Open.as_str(),
                new.priority,
                labels_json,
                new.created_by_session_id.map(|s| s.to_string()),
                new.assignee,
                now,
                now,
            ],
        )?;
        issue_projection_written()?;
        // Construct the just-written value directly so the caller can commit
        // its surrounding transaction before any subsequent read.
        let issue = Issue {
            id,
            project_id: new.project_id,
            display_number,
            title: new.title.clone(),
            body: new.body.clone(),
            status: IssueStatus::Open,
            priority: new.priority,
            labels: new.labels.clone(),
            created_by_session_id: new.created_by_session_id,
            assignee: new.assignee.clone(),
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
            created_at: chrono::DateTime::parse_from_rfc3339(&now)
                .map_err(|e| DaemonError::Store(format!("Invalid generated timestamp: {e}")))?
                .with_timezone(&Utc),
            updated_at: chrono::DateTime::parse_from_rfc3339(&now)
                .map_err(|e| DaemonError::Store(format!("Invalid generated timestamp: {e}")))?
                .with_timezone(&Utc),
            closed_at: None,
            row_version: 1,
            archived_at: None,
        };
        let request = issue_create_semantic_request(issue.id, new);
        let event = build_issue_event(
            issue.clone(),
            1,
            IssueEventOperationV1::Created,
            &actor,
            0,
            request,
            issue.created_at,
        )?;
        insert_issue_event_tx(tx, &event)?;
        Ok(IdempotentIssueCreate {
            issue,
            deduplicated: false,
            create_fields_match: true,
        })
    }

    /// Create a new issue. Allocates the id (lowercase UUID), the monotonic
    /// `display_number`, and both timestamps inside one transaction.
    pub fn create_issue(&self, new: &NewIssue) -> Result<Issue> {
        let id = Uuid::new_v4();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let outcome = Self::idempotent_issue_in_tx(
            &tx,
            id,
            new,
            IssueWriteActor::Operator {
                label: "rsi-rpc:CreateIssue".to_string(),
            },
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(outcome.issue)
    }

    /// Create an issue at a caller-supplied deterministic UUID. This is the
    /// durable idempotency primitive for C5; operator `create_issue` remains
    /// intentionally UUIDv4-backed above.
    pub(crate) fn create_issue_idempotent(
        &self,
        id: Uuid,
        new: &NewIssue,
    ) -> Result<IdempotentIssueCreate> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let outcome = Self::idempotent_issue_in_tx(
            &tx,
            id,
            new,
            IssueWriteActor::System {
                label: "rsi:deterministic-create".to_string(),
            },
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Attributed creation rechecks the server-bound Session project in the
    /// same transaction as deterministic UUID replay. A caller cannot retain
    /// a stale project snapshot across a Session reassignment.
    pub(crate) fn create_agent_issue_idempotent(
        &self,
        caller_session_id: Uuid,
        id: Uuid,
        new: &NewIssue,
    ) -> Result<IdempotentIssueCreate> {
        self.create_agent_issue_idempotent_with_key(caller_session_id, id, new, &id.to_string())
    }

    pub(crate) fn create_agent_issue_idempotent_with_key(
        &self,
        caller_session_id: Uuid,
        id: Uuid,
        new: &NewIssue,
        idempotency_key: &str,
    ) -> Result<IdempotentIssueCreate> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let project_id: Option<String> = tx
            .query_row(
                "SELECT s.project_id FROM sessions s JOIN projects p ON p.id = s.project_id WHERE s.id = ?1",
                params![caller_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if project_id.as_deref() != Some(&new.project_id.to_string())
            || new.created_by_session_id != Some(caller_session_id)
        {
            return Err(DaemonError::Store(format!(
                "agent_create_issue_project_unavailable:{caller_session_id}"
            )));
        }
        let outcome = Self::idempotent_issue_in_tx(
            &tx,
            id,
            new,
            IssueWriteActor::Session {
                session_id: caller_session_id,
                owning_epic_id: None,
                idempotency_key: idempotency_key.to_string(),
            },
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Atomically insert/reactivate the deterministic recovery Resume wake and
    /// create/replay its attributed issue. The session project is re-read in
    /// this transaction so a stale caller snapshot cannot split custody.
    pub(crate) fn settle_master_no_idle_recovery(
        &self,
        caller_session_id: Uuid,
        wake: &ScheduledJob,
        issue: Option<(Uuid, &NewIssue)>,
    ) -> Result<MasterNoIdleStoreRecovery> {
        if wake.wake_mode != WakeMode::Resume
            || wake.wake_session_id != Some(caller_session_id)
            || !matches!(wake.schedule.recurrence, Recurrence::Once)
        {
            return Err(DaemonError::Store(
                "master_no_idle_recovery_requires_same_session_one_shot_resume".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let persisted_project: Option<Option<String>> = tx
            .query_row(
                "SELECT project_id FROM sessions WHERE id = ?1",
                params![caller_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(persisted_project) = persisted_project else {
            return Err(DaemonError::SessionNotFound(caller_session_id));
        };
        if persisted_project.as_deref() != wake.project_id.map(|id| id.to_string()).as_deref() {
            return Err(DaemonError::Store(
                "master_no_idle_recovery_project_context_changed".into(),
            ));
        }

        let wake_deduplicated =
            super::scheduled_jobs::upsert_master_no_idle_recovery_wake_tx(&tx, wake)?;
        master_no_idle_fault_after_wake()?;

        let disposition = match (wake.project_id, issue) {
            (Some(project_id), Some((issue_id, new)))
                if new.project_id == project_id
                    && new.created_by_session_id == Some(caller_session_id) =>
            {
                let outcome = Self::idempotent_issue_in_tx(
                    &tx,
                    issue_id,
                    new,
                    IssueWriteActor::System {
                        label: "rsi:master-no-idle".to_string(),
                    },
                )?;
                let same_canonical_cause = outcome.issue.project_id == new.project_id
                    && outcome.issue.title == new.title
                    && outcome.issue.body == new.body
                    && outcome.issue.priority == new.priority
                    && outcome.issue.labels == new.labels
                    && outcome.issue.assignee == new.assignee
                    && outcome.issue.idea_id == new.idea_id
                    && outcome.issue.source_event_id == new.source_event_id
                    && outcome.issue.source_finding_ref == new.source_finding_ref;
                if !outcome.create_fields_match && !same_canonical_cause {
                    return Err(DaemonError::Store(
                        "master_no_idle_issue_id_conflict".into(),
                    ));
                }
                MasterNoIdleStoreRecovery::WakeAndIssue {
                    issue_id: outcome.issue.id,
                    wake_deduplicated,
                    issue_deduplicated: outcome.deduplicated,
                }
            }
            (None, None) => MasterNoIdleStoreRecovery::ProjectlessWakeOnly { wake_deduplicated },
            _ => {
                return Err(DaemonError::Store(
                    "master_no_idle_recovery_issue_project_mismatch".into(),
                ));
            }
        };
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(disposition)
    }

    /// The sole automatic settlement primitive.  Selection/insertion and
    /// marker resolution happen only after all durable admission predicates
    /// have been read under this same SQLite transaction.
    pub(crate) fn settle_c5_autofile_pending(
        &self,
        source_session_id: Uuid,
        id: Uuid,
        new: &NewIssue,
        pending_key: &str,
    ) -> C5TransitionResult<C5SettlementOutcome> {
        if source_session_id_from_c5_pending_key(pending_key) != Some(source_session_id) {
            return Err(C5TransitionError::Conflict {
                operation: "settlement_pending_key",
                session_id: source_session_id,
            });
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(
            |source| C5TransitionError::Retryable {
                operation: "settlement_begin",
                source: DaemonError::Database(source),
            },
        )?;
        let marker: Option<String> = tx
            .query_row(
                "SELECT value FROM daemon_settings WHERE key = ?1",
                params![pending_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| C5TransitionError::Retryable {
                operation: "settlement_marker_read",
                source: DaemonError::Database(source),
            })?;
        if marker.is_none() {
            commit_c5_transaction(tx, "settlement_stale_commit")?;
            return Ok(C5SettlementOutcome::StaleMarker);
        }
        let marker = C5AutofilePending::parse(marker.as_deref().expect("checked above")).map_err(
            |source| C5TransitionError::InvalidJournal {
                key: pending_key.to_string(),
                source,
            },
        )?;
        if marker.source_session_id != source_session_id {
            return Err(C5TransitionError::Conflict {
                operation: "settlement_pending_source",
                session_id: source_session_id,
            });
        }
        let capacity_owned = super::capacity_recovery::c5_marker_is_capacity_owned_tx(
            &tx,
            source_session_id,
            pending_key,
        )
        .map_err(|source| C5TransitionError::Retryable {
            operation: "settlement_capacity_ownership_read",
            source,
        })?;
        if capacity_owned {
            if tx
                .execute(
                    "DELETE FROM daemon_settings WHERE key=?1",
                    params![pending_key],
                )
                .map_err(|source| C5TransitionError::Retryable {
                    operation: "settlement_capacity_marker_delete",
                    source: DaemonError::Database(source),
                })?
                != 1
            {
                return Err(C5TransitionError::Conflict {
                    operation: "settlement_capacity_marker_delete",
                    session_id: source_session_id,
                });
            }
            commit_c5_transaction(tx, "settlement_capacity_commit")?;
            return Ok(C5SettlementOutcome::Suppressed);
        }
        let source_context: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT s.status, s.project_id
                 FROM sessions s JOIN projects p ON p.id = s.project_id
                 WHERE s.id = ?1",
                params![source_session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|source| C5TransitionError::Retryable {
                operation: "settlement_source_read",
                source: DaemonError::Database(source),
            })?;
        let Some((status, source_project_id)) = source_context else {
            return Err(C5TransitionError::MissingSource {
                session_id: source_session_id,
            });
        };
        if source_project_id.as_deref() != Some(&new.project_id.to_string())
            || new.created_by_session_id != Some(source_session_id)
        {
            return Err(C5TransitionError::Conflict {
                operation: "settlement_project_context",
                session_id: source_session_id,
            });
        }
        if status != "Failed" {
            tx.execute(
                "DELETE FROM daemon_settings WHERE key = ?1",
                params![pending_key],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "settlement_suppression_delete",
                source: DaemonError::Database(source),
            })?;
            commit_c5_transaction(tx, "settlement_suppression_commit")?;
            return Ok(C5SettlementOutcome::Suppressed);
        }
        let successor: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM sessions WHERE continued_from = ?1 LIMIT 1",
                params![source_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| C5TransitionError::Retryable {
                operation: "settlement_successor_read",
                source: DaemonError::Database(source),
            })?;
        if successor.is_some() {
            tx.execute(
                "DELETE FROM daemon_settings WHERE key = ?1",
                params![pending_key],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "settlement_successor_delete",
                source: DaemonError::Database(source),
            })?;
            commit_c5_transaction(tx, "settlement_successor_commit")?;
            return Ok(C5SettlementOutcome::Suppressed);
        }
        let outcome = Self::idempotent_issue_in_tx(
            &tx,
            id,
            new,
            IssueWriteActor::System {
                label: "rsi:c5-autofile".to_string(),
            },
        )
        .map_err(|source| C5TransitionError::Retryable {
            operation: "settlement_issue_insert_or_select",
            source,
        })?;
        if tx
            .execute(
                "DELETE FROM daemon_settings WHERE key = ?1",
                params![pending_key],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "settlement_marker_delete",
                source: DaemonError::Database(source),
            })?
            != 1
        {
            return Err(C5TransitionError::Conflict {
                operation: "settlement_marker_delete",
                session_id: source_session_id,
            });
        }
        issue_write_before_commit().map_err(|source| C5TransitionError::Retryable {
            operation: "settlement_before_commit",
            source,
        })?;
        commit_c5_transaction(tx, "settlement_commit")?;
        Ok(C5SettlementOutcome::Committed {
            deduplicated: outcome.deduplicated,
        })
    }

    /// Get an issue by ID.
    pub fn get_issue(&self, id: Uuid) -> Result<Option<Issue>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE id = ?1"))?;
        let mut rows = stmt.query_map(params![id.to_string()], map_issue_row)?;
        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_issue()?)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// List issues, filtered by status / creator with an optional limit.
    /// Ordered by `display_number` (creation order).
    pub fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>> {
        let mut sql = format!("SELECT {ISSUE_COLUMNS} FROM issues");
        let mut clauses: Vec<&str> = Vec::new();
        let mut bind: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(project_id) = filter.project_id {
            clauses.push("project_id = ?");
            bind.push(project_id.to_string().into());
        }
        if let Some(idea_id) = filter.idea_id {
            clauses.push("idea_id = ?");
            bind.push(idea_id.to_string().into());
        }
        if let Some(source_event_id) = filter.source_event_id {
            clauses.push("source_event_id = ?");
            bind.push(source_event_id.to_string().into());
        }
        if let Some(source_finding_ref) = &filter.source_finding_ref {
            clauses.push("source_finding_ref = ?");
            bind.push(source_finding_ref.as_str().to_string().into());
        }
        if let Some(status) = filter.status {
            clauses.push("status = ?");
            bind.push(status.as_str().to_string().into());
        }
        if let Some(creator) = filter.created_by_session_id {
            clauses.push("created_by_session_id = ?");
            bind.push(creator.to_string().into());
        }
        match filter.archive {
            Some(IssueArchiveFilterV1::Active) => clauses.push("archived_at IS NULL"),
            Some(IssueArchiveFilterV1::Archived) => clauses.push("archived_at IS NOT NULL"),
            Some(IssueArchiveFilterV1::All) | None => {}
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY display_number ASC");
        if let Some(limit) = filter.limit {
            sql.push_str(" LIMIT ?");
            bind.push(i64::from(limit).into());
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind), map_issue_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|row| row.into_issue()).collect()
    }

    /// Bounded, project-scoped Issue workspace page. Readiness and relationship
    /// counts are projected here so callers never need per-row RPCs.
    pub(crate) fn list_issue_workspace_page(
        &self,
        request: &ListIssuesPageRequestV1,
    ) -> Result<IssueWorkspacePageV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(IssueWorkspaceErrorCodeV1::InvalidRequest, None, None, None)
        })?;
        if let Some(cursor) = &request.cursor {
            let cursor_issue = self
                .conn
                .query_row(
                    &format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE id=?1 AND project_id=?2"),
                    params![
                        match cursor {
                            IssueWorkspaceCursorV1::UpdatedDesc { issue_id, .. }
                            | IssueWorkspaceCursorV1::DisplayNumberAsc { issue_id, .. }
                            | IssueWorkspaceCursorV1::PriorityAsc { issue_id, .. } => {
                                issue_id.to_string()
                            }
                        },
                        request.project_id.to_string()
                    ],
                    map_issue_row,
                )
                .optional()?
                .map(IssueRow::into_issue)
                .transpose()?;
            if cursor_issue
                .as_ref()
                .map(|issue| issue_workspace_cursor(issue, request.sort))
                .as_ref()
                != Some(cursor)
            {
                return Err(issue_workspace_error(
                    IssueWorkspaceErrorCodeV1::InvalidRequest,
                    None,
                    None,
                    None,
                ));
            }
        }
        let mut values = Vec::<rusqlite::types::Value>::new();
        let project = bind_value(&mut values, request.project_id.to_string());
        let mut clauses = vec![format!("i.project_id={project}")];

        match request.archive {
            IssueArchiveFilterV1::Active => clauses.push("i.archived_at IS NULL".into()),
            IssueArchiveFilterV1::Archived => clauses.push("i.archived_at IS NOT NULL".into()),
            IssueArchiveFilterV1::All => {}
        }
        if !request.statuses.is_empty() {
            let binds = request
                .statuses
                .iter()
                .map(|status| bind_value(&mut values, status.as_str().to_string()))
                .collect::<Vec<_>>()
                .join(",");
            clauses.push(format!("i.status IN ({binds})"));
        }
        if !request.priorities.is_empty() {
            let binds = request
                .priorities
                .iter()
                .map(|priority| bind_value(&mut values, i64::from(*priority)))
                .collect::<Vec<_>>()
                .join(",");
            clauses.push(format!("i.priority IN ({binds})"));
        }
        if let Some(assignee) = &request.assignee {
            let bind = bind_value(&mut values, assignee.clone());
            clauses.push(format!("i.assignee={bind}"));
        } else if request.unassigned {
            clauses.push("i.assignee IS NULL".into());
        }
        for label in &request.labels_all {
            let bind = bind_value(&mut values, label.clone());
            clauses.push(format!(
                "EXISTS(SELECT 1 FROM json_each(i.labels) label WHERE label.value={bind})"
            ));
        }
        if !request.provenance.is_empty() {
            let provenance = request
                .provenance
                .iter()
                .map(|kind| match kind {
                    IssueWorkspaceProvenanceV1::Operator => "i.created_by_session_id IS NULL",
                    IssueWorkspaceProvenanceV1::Session => "i.created_by_session_id IS NOT NULL",
                    IssueWorkspaceProvenanceV1::Idea => "i.idea_id IS NOT NULL",
                    IssueWorkspaceProvenanceV1::Finding => "i.source_finding_ref IS NOT NULL",
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            clauses.push(format!("({provenance})"));
        }
        if let Some(query) = &request.query {
            let bind = bind_value(&mut values, format!("%{query}%"));
            clauses.push(format!("(i.title LIKE {bind} OR i.body LIKE {bind})"));
        }

        const OPEN_BLOCKERS: &str = "SELECT COUNT(*) FROM issue_deps d \
             JOIN issues b ON b.id=d.depends_on_id AND b.project_id=d.project_id \
             WHERE d.project_id=i.project_id AND d.issue_id=i.id \
               AND b.archived_at IS NULL AND b.status IN ('Open','InProgress')";
        match request.readiness {
            IssueWorkspaceReadinessFilterV1::Any => {}
            IssueWorkspaceReadinessFilterV1::Ready => clauses.push(format!(
                "i.archived_at IS NULL AND i.status='Open' AND ({OPEN_BLOCKERS})=0"
            )),
            IssueWorkspaceReadinessFilterV1::Blocked => clauses.push(format!(
                "i.archived_at IS NULL AND i.status='Open' AND ({OPEN_BLOCKERS})>0"
            )),
        }

        if let Some(cursor) = &request.cursor {
            let forward = request.direction == PageDirectionV1::Forward;
            let predicate = match cursor {
                IssueWorkspaceCursorV1::UpdatedDesc {
                    updated_at,
                    display_number,
                    issue_id,
                } => {
                    let time = bind_value(&mut values, canonical_timestamp(*updated_at));
                    let number = bind_value(&mut values, *display_number);
                    let id = bind_value(&mut values, issue_id.to_string());
                    if forward {
                        format!(
                            "(i.updated_at<{time} OR (i.updated_at={time} AND (i.display_number>{number} OR (i.display_number={number} AND i.id>{id}))))"
                        )
                    } else {
                        format!(
                            "(i.updated_at>{time} OR (i.updated_at={time} AND (i.display_number<{number} OR (i.display_number={number} AND i.id<{id}))))"
                        )
                    }
                }
                IssueWorkspaceCursorV1::DisplayNumberAsc {
                    display_number,
                    issue_id,
                } => {
                    let number = bind_value(&mut values, *display_number);
                    let id = bind_value(&mut values, issue_id.to_string());
                    let operator = if forward { ">" } else { "<" };
                    format!(
                        "(i.display_number{operator}{number} OR (i.display_number={number} AND i.id{operator}{id}))"
                    )
                }
                IssueWorkspaceCursorV1::PriorityAsc {
                    priority,
                    display_number,
                    issue_id,
                } => {
                    let null_rank = bind_value(&mut values, i64::from(priority.is_none()));
                    let priority = bind_value(&mut values, i64::from(priority.unwrap_or(0)));
                    let number = bind_value(&mut values, *display_number);
                    let id = bind_value(&mut values, issue_id.to_string());
                    let operator = if forward { ">" } else { "<" };
                    format!(
                        "((i.priority IS NULL){operator}{null_rank} OR \
                         ((i.priority IS NULL)={null_rank} AND (COALESCE(i.priority,0){operator}{priority} OR \
                         (COALESCE(i.priority,0)={priority} AND (i.display_number{operator}{number} OR \
                         (i.display_number={number} AND i.id{operator}{id}))))))"
                    )
                }
            };
            clauses.push(predicate);
        }

        let forward = request.direction == PageDirectionV1::Forward;
        let order = match (request.sort, forward) {
            (IssueWorkspaceSortV1::UpdatedDesc, true) => {
                "i.updated_at DESC, i.display_number ASC, i.id ASC"
            }
            (IssueWorkspaceSortV1::UpdatedDesc, false) => {
                "i.updated_at ASC, i.display_number DESC, i.id DESC"
            }
            (IssueWorkspaceSortV1::DisplayNumberAsc, true) => "i.display_number ASC, i.id ASC",
            (IssueWorkspaceSortV1::DisplayNumberAsc, false) => "i.display_number DESC, i.id DESC",
            (IssueWorkspaceSortV1::PriorityAsc, true) => {
                "(i.priority IS NULL) ASC, i.priority ASC, i.display_number ASC, i.id ASC"
            }
            (IssueWorkspaceSortV1::PriorityAsc, false) => {
                "(i.priority IS NULL) DESC, i.priority DESC, i.display_number DESC, i.id DESC"
            }
        };
        let limit = bind_value(&mut values, i64::from(request.limit) + 1);
        let sql = format!(
            "SELECT {QUALIFIED_ISSUE_COLUMNS}, \
                    ({OPEN_BLOCKERS}) AS open_blocker_count, \
                    (SELECT COUNT(*) FROM issue_deps d WHERE d.project_id=i.project_id AND d.depends_on_id=i.id) AS dependent_count \
             FROM issues i WHERE {} ORDER BY {order} LIMIT {limit}",
            clauses.join(" AND ")
        );
        let rows = self
            .conn
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                Ok((
                    map_issue_row(row)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = rows
            .into_iter()
            .map(|(row, open_blocker_count, dependent_count)| {
                let issue = row.into_issue()?;
                let readiness = if issue.archived_at.is_some() || issue.status != IssueStatus::Open
                {
                    IssueWorkspaceReadinessV1::NotEligible
                } else if open_blocker_count > 0 {
                    IssueWorkspaceReadinessV1::Blocked
                } else {
                    IssueWorkspaceReadinessV1::Ready
                };
                Ok(IssueWorkspaceRowV1 {
                    issue,
                    readiness,
                    open_blocker_count: u32::try_from(open_blocker_count).map_err(|_| {
                        DaemonError::Store("Issue blocker count is out of range".into())
                    })?,
                    dependent_count: u32::try_from(dependent_count).map_err(|_| {
                        DaemonError::Store("Issue dependent count is out of range".into())
                    })?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let has_more = result.len() > request.limit as usize;
        if has_more {
            result.pop();
        }
        if !forward {
            result.reverse();
        }
        let first = result
            .first()
            .map(|row| issue_workspace_cursor(&row.issue, request.sort));
        let last = result
            .last()
            .map(|row| issue_workspace_cursor(&row.issue, request.sort));
        let (previous_cursor, next_cursor) = if forward {
            (
                request.cursor.as_ref().and(first),
                has_more.then_some(last).flatten(),
            )
        } else {
            (
                has_more.then_some(first).flatten(),
                request.cursor.as_ref().and(last),
            )
        };
        Ok(IssueWorkspacePageV1 {
            rows: result,
            previous_cursor,
            next_cursor,
        })
    }

    pub(crate) fn get_issue_workspace_in_project(
        &self,
        request: &GetIssueInProjectRequestV1,
    ) -> Result<Option<IssueWorkspaceRowV1>> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = self.conn.unchecked_transaction()?;
        let Some(issue) = Self::get_issue_in_project_tx(&tx, request.project_id, request.issue_id)?
        else {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM issues WHERE id=?1)",
                params![request.issue_id.to_string()],
                |row| row.get(0),
            )?;
            tx.commit()?;
            if exists {
                return Err(issue_workspace_error(
                    IssueWorkspaceErrorCodeV1::NotFoundInProject,
                    Some(request.issue_id),
                    None,
                    None,
                ));
            }
            return Ok(None);
        };
        let open_blocker_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM issue_deps d
             JOIN issues b ON b.id=d.depends_on_id AND b.project_id=d.project_id
             WHERE d.project_id=?1 AND d.issue_id=?2 AND b.archived_at IS NULL
               AND b.status IN ('Open','InProgress')",
            params![request.project_id.to_string(), request.issue_id.to_string()],
            |row| row.get(0),
        )?;
        let dependent_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM issue_deps WHERE project_id=?1 AND depends_on_id=?2",
            params![request.project_id.to_string(), request.issue_id.to_string()],
            |row| row.get(0),
        )?;
        tx.commit()?;
        let readiness = if issue.archived_at.is_some() || issue.status != IssueStatus::Open {
            IssueWorkspaceReadinessV1::NotEligible
        } else if open_blocker_count > 0 {
            IssueWorkspaceReadinessV1::Blocked
        } else {
            IssueWorkspaceReadinessV1::Ready
        };
        Ok(Some(IssueWorkspaceRowV1 {
            issue,
            readiness,
            open_blocker_count: u32::try_from(open_blocker_count)
                .map_err(|_| DaemonError::Store("Issue blocker count is out of range".into()))?,
            dependent_count: u32::try_from(dependent_count)
                .map_err(|_| DaemonError::Store("Issue dependent count is out of range".into()))?,
        }))
    }

    pub(crate) fn list_issue_workspace_dependencies(
        &self,
        request: &ListIssueDependenciesRequestV1,
    ) -> Result<IssueDependencyPageV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = self.conn.unchecked_transaction()?;
        if Self::get_issue_in_project_tx(&tx, request.project_id, request.issue_id)?.is_none() {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NotFoundInProject,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        if let Some(cursor) = &request.cursor {
            let relation = match request.direction {
                IssueDependencyDirectionV1::BlockedBy => {
                    "d.issue_id=?2 AND d.depends_on_id=?3 AND i.id=d.depends_on_id"
                }
                IssueDependencyDirectionV1::Blocks => {
                    "d.depends_on_id=?2 AND d.issue_id=?3 AND i.id=d.issue_id"
                }
            };
            let stored_display_number = tx
                .query_row(
                    &format!(
                        "SELECT i.display_number FROM issue_deps d \
                         JOIN issues i ON i.project_id=d.project_id \
                         WHERE d.project_id=?1 AND {relation}"
                    ),
                    params![
                        request.project_id.to_string(),
                        request.issue_id.to_string(),
                        cursor.issue_id.to_string()
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if stored_display_number != Some(cursor.display_number) {
                return Err(issue_workspace_error(
                    IssueWorkspaceErrorCodeV1::InvalidRequest,
                    Some(request.issue_id),
                    None,
                    None,
                ));
            }
        }
        let mut values = vec![
            request.project_id.to_string().into(),
            request.issue_id.to_string().into(),
        ];
        let relation = match request.direction {
            IssueDependencyDirectionV1::BlockedBy => "d.issue_id=?2 AND i.id=d.depends_on_id",
            IssueDependencyDirectionV1::Blocks => "d.depends_on_id=?2 AND i.id=d.issue_id",
        };
        let mut cursor_clause = String::new();
        if let Some(cursor) = &request.cursor {
            let number = bind_value(&mut values, cursor.display_number);
            let id = bind_value(&mut values, cursor.issue_id.to_string());
            let operator = if request.page_direction == PageDirectionV1::Forward {
                ">"
            } else {
                "<"
            };
            cursor_clause = format!(
                " AND (i.display_number{operator}{number} OR (i.display_number={number} AND i.id{operator}{id}))"
            );
        }
        let forward = request.page_direction == PageDirectionV1::Forward;
        let order = if forward {
            "i.display_number ASC, i.id ASC"
        } else {
            "i.display_number DESC, i.id DESC"
        };
        let limit = bind_value(&mut values, i64::from(request.limit) + 1);
        let sql = format!(
            "SELECT {QUALIFIED_ISSUE_COLUMNS}, d.project_id, d.issue_id, d.depends_on_id, d.created_at
             FROM issue_deps d JOIN issues i ON i.project_id=d.project_id
             WHERE d.project_id=?1 AND {relation}{cursor_clause}
             ORDER BY {order} LIMIT {limit}"
        );
        let rows = tx
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                Ok((
                    map_issue_row(row)?,
                    row.get::<_, String>(18)?,
                    row.get::<_, String>(19)?,
                    row.get::<_, String>(20)?,
                    row.get::<_, String>(21)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut items = rows
            .into_iter()
            .map(|(issue, project, dependent, blocker, created)| {
                Ok(IssueDependencyItemV1 {
                    dependency: dep_from_row((project, dependent, blocker, created))?,
                    related_issue: issue.into_issue()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let has_more = items.len() > request.limit as usize;
        if has_more {
            items.pop();
        }
        if !forward {
            items.reverse();
        }
        let first = items.first().map(dependency_cursor);
        let last = items.last().map(dependency_cursor);
        let (previous_cursor, next_cursor) = if forward {
            (
                request.cursor.as_ref().and(first),
                has_more.then_some(last).flatten(),
            )
        } else {
            (
                has_more.then_some(first).flatten(),
                request.cursor.as_ref().and(last),
            )
        };
        tx.commit()?;
        Ok(IssueDependencyPageV1 {
            items,
            previous_cursor,
            next_cursor,
        })
    }

    pub(crate) fn list_issue_workspace_events(
        &self,
        request: &ListIssueEventsV2RequestV1,
    ) -> Result<IssueEventPageV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = self.conn.unchecked_transaction()?;
        if Self::get_issue_in_project_tx(&tx, request.project_id, request.issue_id)?.is_none() {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NotFoundInProject,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        let after = request.after_sequence.unwrap_or(0);
        let mut events = tx
            .prepare(&format!(
                "SELECT {ISSUE_EVENT_COLUMNS} FROM issue_events
                 WHERE project_id=?1 AND issue_id=?2 AND sequence>?3
                 ORDER BY sequence ASC LIMIT ?4"
            ))?
            .query_map(
                params![
                    request.project_id.to_string(),
                    request.issue_id.to_string(),
                    after,
                    i64::from(request.limit) + 1,
                ],
                issue_event_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next_after_sequence = if events.len() > request.limit as usize {
            events.pop();
            events.last().map(|event| event.sequence)
        } else {
            None
        };
        tx.commit()?;
        Ok(IssueEventPageV1 {
            events,
            next_after_sequence,
        })
    }

    /// Apply an [`IssueUpdate`] patch (see its docs for the clear-vs-skip
    /// encoding) and bump `updated_at`. Unknown id is an error, not a no-op.
    /// Status changes go through [`Store::update_issue_status`].
    ///
    /// The read-modify-write runs inside one transaction (consistent with
    /// `create_issue`/`add_issue_dep`), making a lost update impossible by
    /// construction even if store access patterns ever change.
    pub fn update_issue(&self, id: Uuid, patch: &IssueUpdate) -> Result<Issue> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let current = Self::get_issue_tx(&tx, id)?
            .ok_or_else(|| DaemonError::Store(format!("Issue not found: {}", id)))?;

        let title = patch.title.clone().unwrap_or_else(|| current.title.clone());
        let body = patch.body.clone().unwrap_or_else(|| current.body.clone());
        let priority = match patch.priority {
            Some(p) => p,             // Some(None)=clear, Some(Some(v))=set
            None => current.priority, // skip
        };
        let labels = patch
            .labels
            .clone()
            .unwrap_or_else(|| current.labels.clone());
        let assignee = match &patch.assignee {
            Some(a) => a.clone(),             // Some(None)=clear, Some(Some(v))=set
            None => current.assignee.clone(), // skip
        };
        let labels_json = serde_json::to_string(&labels)?;

        let now = now_nanos();
        let changed = tx.execute(
            "UPDATE issues SET title = ?1, body = ?2, priority = ?3, labels = ?4,
                               assignee = ?5, updated_at = ?6, row_version = row_version + 1
             WHERE id = ?7 AND row_version = ?8",
            params![
                &title,
                &body,
                priority,
                &labels_json,
                &assignee,
                &now,
                id.to_string(),
                current.row_version,
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "Issue content update lost row-version CAS".into(),
            ));
        }
        issue_projection_written()?;
        let mut updated = current.clone();
        updated.title = title;
        updated.body = body;
        updated.priority = priority;
        updated.labels = labels;
        updated.assignee = assignee;
        updated.updated_at = parse_timestamp(&now).map_err(DaemonError::Store)?;
        updated.row_version += 1;
        let event = build_issue_event(
            updated.clone(),
            updated.row_version,
            IssueEventOperationV1::ContentUpdated,
            &IssueWriteActor::Operator {
                label: "rsi:Store::update_issue".to_string(),
            },
            current.row_version,
            IssueSemanticRequestV1::new(IssueSemanticOperationV1::ContentUpdated {
                issue_id: id,
                expected_row_version: current.row_version,
                patch: IssueContentPatchV1 {
                    title: patch.title.clone(),
                    body: patch.body.clone(),
                    labels: patch.labels.clone(),
                    priority: patch.priority.flatten(),
                    clear_priority: matches!(patch.priority, Some(None)),
                    assignee: patch.assignee.clone().flatten(),
                    clear_assignee: matches!(patch.assignee, Some(None)),
                },
            }),
            updated.updated_at,
        )?;
        insert_issue_event_tx(&tx, &event)?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(updated)
    }

    /// Transition an issue's status. Entering `Closed`/`Cancelled` sets
    /// `closed_at` (preserved across terminal→terminal moves); leaving a
    /// terminal status clears it. Bumps `updated_at`. Unknown id is an error.
    fn update_issue_status_scoped(
        &self,
        project_id: Option<Uuid>,
        id: Uuid,
        status: IssueStatus,
        actor: IssueWriteActor,
    ) -> Result<Issue> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = match project_id {
            Some(project_id) => Self::get_issue_in_project_tx(&tx, project_id, id)?,
            None => Self::get_issue_tx(&tx, id)?,
        }
        .ok_or_else(|| match project_id {
            Some(_) => DaemonError::Store("Issue not found in project".to_string()),
            None => DaemonError::Store(format!("Issue not found: {id}")),
        })?;

        let now = now_nanos();
        let closed_at = if status.is_terminal() {
            Some(
                current
                    .closed_at
                    .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Nanos, true))
                    .unwrap_or_else(|| now.clone()),
            )
        } else {
            None
        };

        let changed = tx.execute(
            "UPDATE issues SET status = ?1, closed_at = ?2, updated_at = ?3, row_version = row_version + 1
             WHERE id = ?4 AND row_version = ?5",
            params![status.as_str(), &closed_at, &now, id.to_string(), current.row_version],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "Issue status update lost row-version CAS".into(),
            ));
        }
        issue_projection_written()?;
        let mut updated = current.clone();
        updated.status = status;
        updated.closed_at = closed_at
            .as_deref()
            .map(parse_timestamp)
            .transpose()
            .map_err(DaemonError::Store)?;
        updated.updated_at = parse_timestamp(&now).map_err(DaemonError::Store)?;
        updated.row_version += 1;
        let event = build_issue_event(
            updated.clone(),
            updated.row_version,
            IssueEventOperationV1::StatusUpdated,
            &actor,
            current.row_version,
            IssueSemanticRequestV1::new(IssueSemanticOperationV1::StatusUpdated {
                issue_id: id,
                expected_row_version: current.row_version,
                status,
            }),
            updated.updated_at,
        )?;
        insert_issue_event_tx(&tx, &event)?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn update_issue_status(&self, id: Uuid, status: IssueStatus) -> Result<Issue> {
        self.update_issue_status_scoped(
            None,
            id,
            status,
            IssueWriteActor::Operator {
                label: "rsi:UpdateIssueStatus".to_string(),
            },
        )
    }

    /// Read an issue only when it is owned by the supplied project.
    pub fn get_issue_in_project(&self, project_id: Uuid, id: Uuid) -> Result<Option<Issue>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ISSUE_COLUMNS} FROM issues WHERE id = ?1 AND project_id = ?2"
        ))?;
        let mut rows = stmt.query(params![id.to_string(), project_id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_issue_row(row)?.into_issue()?)),
            None => Ok(None),
        }
    }

    fn agent_issue_invalid<T>(message: impl Into<String>) -> Result<T> {
        let _ = message.into();
        Err(agent_issue_error(
            rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
            None,
            None,
        ))
    }

    /// Guarded Issue authority for reads and the four mutations: the current
    /// owning-Epic lead (checked first), or the current appointed manager
    /// holding the V2 `IssueCoordinate` grant. Resolved first in every guarded
    /// transaction, so a refused caller writes no Issue row and no event.
    fn resolve_agent_issue_authority(
        tx: &Transaction<'_>,
        caller_session_id: Uuid,
    ) -> Result<AgentIssueAuthority> {
        Self::resolve_agent_issue_authority_tx(tx, caller_session_id)
            .or_else(|_| Self::resolve_agent_issue_coordinator_authority_tx(tx, caller_session_id))
            .map_err(|_| {
                agent_issue_error(
                    rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                    None,
                    None,
                )
            })
    }

    /// The current appointed manager (the committed lineage tip of the
    /// appointment anchor, per `current_manager_session_on`, the same
    /// definition as `HarnessManagerConfigV1::current_session_id`) holding the
    /// V2 `IssueCoordinate` grant bound to the exact appointment scope
    /// version. Every read goes through the Issue transaction.
    fn resolve_agent_issue_coordinator_authority_tx(
        tx: &Transaction<'_>,
        caller_session_id: Uuid,
    ) -> Result<AgentIssueAuthority> {
        use rsi_common::harness_manager_v2::ManagerCapabilityV2;

        let denied = || DaemonError::PolicyDenied("agent_issue_authority_denied".into());
        let project_id: Option<String> = tx
            .query_row(
                "SELECT project_id FROM sessions WHERE id=?1",
                [caller_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(denied)?;
        let project_id = project_id
            .map(|project| Uuid::parse_str(&project).map_err(|_| denied()))
            .transpose()?
            .ok_or_else(denied)?;
        let scope: Option<(String, i64)> = tx
            .query_row(
                "SELECT manager_session_id,row_version FROM harness_manager_scopes WHERE project_id=?1",
                [project_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (manager_anchor, scope_version) = scope.ok_or_else(denied)?;
        let manager_anchor_id = Uuid::parse_str(&manager_anchor).map_err(|_| denied())?;
        if super::harness_manager::current_manager_session_on(tx, project_id, manager_anchor_id)
            != Some(caller_session_id)
        {
            return Err(denied());
        }
        let grant: Option<(String, i64, String)> = tx.query_row(
            "SELECT manager_session_id,scope_version,policy_json FROM harness_manager_v2_policies WHERE project_id=?1",
            [project_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let (grant_manager, grant_scope, policy_json) = grant.ok_or_else(denied)?;
        let policy: rsi_common::harness_manager_v2::ManagerPolicyV2 =
            serde_json::from_str(&policy_json).map_err(|_| denied())?;
        if grant_manager != manager_anchor
            || grant_scope != scope_version
            || !policy
                .capabilities
                .contains(&ManagerCapabilityV2::IssueCoordinate)
        {
            return Err(denied());
        }
        Ok(AgentIssueAuthority {
            caller_session_id,
            project_id,
            actor: AgentIssueActor::Manager { scope_version },
        })
    }

    fn get_issue_in_project_tx(
        tx: &Transaction<'_>,
        project_id: Uuid,
        id: Uuid,
    ) -> Result<Option<Issue>> {
        tx.query_row(
            &format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE id=?1 AND project_id=?2"),
            params![id.to_string(), project_id.to_string()],
            map_issue_row,
        )
        .optional()?
        .map(IssueRow::into_issue)
        .transpose()
    }

    fn event_by_actor_key_tx(
        tx: &Transaction<'_>,
        actor_session_id: Uuid,
        idempotency_key: &str,
    ) -> Result<Option<IssueEventV1>> {
        tx.query_row(
            &format!(
                "SELECT {ISSUE_EVENT_COLUMNS} FROM issue_events
                 WHERE actor_session_id=?1 AND idempotency_key=?2"
            ),
            params![actor_session_id.to_string(), idempotency_key],
            issue_event_from_row,
        )
        .optional()
        .map_err(DaemonError::Database)
    }

    fn event_by_id_tx(tx: &Transaction<'_>, event_id: Uuid) -> Result<Option<IssueEventV1>> {
        tx.query_row(
            &format!("SELECT {ISSUE_EVENT_COLUMNS} FROM issue_events WHERE id=?1"),
            params![event_id.to_string()],
            issue_event_from_row,
        )
        .optional()
        .map_err(DaemonError::Database)
    }

    fn operator_replay_tx(
        tx: &Transaction<'_>,
        project_id: Uuid,
        issue_id: Uuid,
        expected_row_version: i64,
        idempotency_key: &str,
        operation: IssueEventOperationV1,
        request: &IssueSemanticRequestV1,
    ) -> Result<Option<OperatorIssueMutationResultV1>> {
        let event_id = operator_issue_mutation_event_id(issue_id, idempotency_key);
        let Some(event) = Self::event_by_id_tx(tx, event_id)? else {
            return Ok(None);
        };
        let fingerprint = request.fingerprint().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(issue_id),
                None,
                None,
            )
        })?;
        if event.project_id != project_id
            || event.issue_id != issue_id
            || event.operation != operation
            || event.expected_row_version != expected_row_version
            || event.request_fingerprint != fingerprint
            || event.actor_kind != IssueActorKindV1::Operator
            || event.idempotency_key.is_some()
        {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::IdempotencyConflict,
                Some(issue_id),
                None,
                None,
            ));
        }
        Ok(Some(OperatorIssueMutationResultV1 {
            issue: event.issue.clone(),
            event,
            deduplicated: true,
        }))
    }

    fn operator_issue_projection_cas_tx(
        tx: &Transaction<'_>,
        prior: &Issue,
        next: &Issue,
        operation: IssueEventOperationV1,
        request: IssueSemanticRequestV1,
        idempotency_key: &str,
        actor_label: &str,
    ) -> Result<OperatorIssueMutationResultV1> {
        let changed = match operation {
            IssueEventOperationV1::ContentUpdated => tx.execute(
                "UPDATE issues
                 SET title=?1, body=?2, labels=?3, priority=?4, assignee=?5,
                     updated_at=?6, row_version=row_version+1
                 WHERE id=?7 AND project_id=?8 AND row_version=?9",
                params![
                    &next.title,
                    &next.body,
                    serde_json::to_string(&next.labels)?,
                    next.priority,
                    next.assignee.as_deref(),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    prior.project_id.to_string(),
                    prior.row_version,
                ],
            )?,
            IssueEventOperationV1::StatusUpdated => tx.execute(
                "UPDATE issues
                 SET status=?1, closed_at=?2, updated_at=?3, row_version=row_version+1
                 WHERE id=?4 AND project_id=?5 AND row_version=?6",
                params![
                    next.status.as_str(),
                    next.closed_at.map(canonical_timestamp),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    prior.project_id.to_string(),
                    prior.row_version,
                ],
            )?,
            IssueEventOperationV1::Archived | IssueEventOperationV1::Restored => tx.execute(
                "UPDATE issues
                 SET archived_at=?1, updated_at=?2, row_version=row_version+1
                 WHERE id=?3 AND project_id=?4 AND row_version=?5",
                params![
                    next.archived_at.map(canonical_timestamp),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    prior.project_id.to_string(),
                    prior.row_version,
                ],
            )?,
            _ => {
                return Err(issue_workspace_error(
                    IssueWorkspaceErrorCodeV1::InvalidRequest,
                    Some(prior.id),
                    None,
                    None,
                ));
            }
        };
        if changed != 1 {
            let actual = Self::get_issue_in_project_tx(tx, prior.project_id, prior.id)?
                .map(|issue| issue.row_version);
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::StaleVersion,
                Some(prior.id),
                Some(prior.row_version),
                actual,
            ));
        }
        issue_projection_written()?;
        let actor = IssueWriteActor::OperatorIdempotent {
            label: actor_label.to_string(),
            event_id: operator_issue_mutation_event_id(prior.id, idempotency_key),
        };
        let event = build_issue_event(
            next.clone(),
            next.row_version,
            operation,
            &actor,
            prior.row_version,
            request,
            next.updated_at,
        )?;
        insert_issue_event_tx(tx, &event)?;
        Ok(OperatorIssueMutationResultV1 {
            issue: next.clone(),
            event,
            deduplicated: false,
        })
    }

    pub(crate) fn create_issue_workspace(
        &self,
        request: &CreateIssueV2RequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(IssueWorkspaceErrorCodeV1::InvalidRequest, None, None, None)
        })?;
        let issue_id = operator_issue_create_id(request.project_id, &request.idempotency_key);
        let event_id = operator_issue_create_event_id(issue_id);
        let new = NewIssue {
            project_id: request.project_id,
            title: request.title.clone(),
            body: request.body.clone(),
            priority: request.priority,
            labels: request.labels.clone(),
            created_by_session_id: None,
            assignee: request.assignee.clone(),
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let project_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
            params![request.project_id.to_string()],
            |row| row.get(0),
        )?;
        if !project_exists {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NotFoundInProject,
                Some(issue_id),
                None,
                None,
            ));
        }
        let outcome = Self::idempotent_issue_in_tx(
            &tx,
            issue_id,
            &new,
            IssueWriteActor::OperatorIdempotent {
                label: "rsi-rpc:CreateIssueV2".to_string(),
                event_id,
            },
        )?;
        if !outcome.create_fields_match {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::IdempotencyConflict,
                Some(issue_id),
                None,
                None,
            ));
        }
        let event = Self::event_by_id_tx(&tx, event_id)?.ok_or_else(|| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::IdempotencyConflict,
                Some(issue_id),
                None,
                None,
            )
        })?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(OperatorIssueMutationResultV1 {
            issue: outcome.issue,
            event,
            deduplicated: outcome.deduplicated,
        })
    }

    pub(crate) fn update_issue_workspace(
        &self,
        request: &UpdateIssueRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let semantic_request = request.semantic_request().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(replay) = Self::operator_replay_tx(
            &tx,
            request.project_id,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            IssueEventOperationV1::ContentUpdated,
            &semantic_request,
        )? {
            issue_write_before_commit()?;
            tx.commit()?;
            return Ok(replay);
        }
        let prior = Self::get_issue_in_project_tx(&tx, request.project_id, request.issue_id)?
            .ok_or_else(|| {
                issue_workspace_error(
                    IssueWorkspaceErrorCodeV1::NotFoundInProject,
                    Some(request.issue_id),
                    None,
                    None,
                )
            })?;
        if prior.row_version != request.expected_row_version {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::StaleVersion,
                Some(request.issue_id),
                Some(request.expected_row_version),
                Some(prior.row_version),
            ));
        }
        if prior.archived_at.is_some()
            || !matches!(prior.status, IssueStatus::Open | IssueStatus::InProgress)
        {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidTransition,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        let title = request
            .patch
            .title
            .clone()
            .unwrap_or_else(|| prior.title.clone());
        let body = request
            .patch
            .body
            .clone()
            .unwrap_or_else(|| prior.body.clone());
        let labels = request
            .patch
            .labels
            .clone()
            .unwrap_or_else(|| prior.labels.clone());
        let priority = match request.patch.priority {
            NullablePatchV1::Unchanged => prior.priority,
            NullablePatchV1::Clear => None,
            NullablePatchV1::Set(value) => Some(value),
        };
        let assignee = match &request.patch.assignee {
            NullablePatchV1::Unchanged => prior.assignee.clone(),
            NullablePatchV1::Clear => None,
            NullablePatchV1::Set(value) => Some(value.clone()),
        };
        if title == prior.title
            && body == prior.body
            && labels == prior.labels
            && priority == prior.priority
            && assignee == prior.assignee
        {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NoSemanticChange,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        let now = Utc::now();
        let mut next = prior.clone();
        next.title = title;
        next.body = body;
        next.labels = labels;
        next.priority = priority;
        next.assignee = assignee;
        next.updated_at = now;
        next.row_version += 1;
        let result = Self::operator_issue_projection_cas_tx(
            &tx,
            &prior,
            &next,
            IssueEventOperationV1::ContentUpdated,
            semantic_request,
            &request.idempotency_key,
            "rsi-rpc:UpdateIssue",
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn update_issue_status_workspace(
        &self,
        request: &UpdateIssueStatusV2RequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let semantic_request = request.semantic_request().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(replay) = Self::operator_replay_tx(
            &tx,
            request.project_id,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            IssueEventOperationV1::StatusUpdated,
            &semantic_request,
        )? {
            issue_write_before_commit()?;
            tx.commit()?;
            return Ok(replay);
        }
        let prior = Self::get_issue_in_project_tx(&tx, request.project_id, request.issue_id)?
            .ok_or_else(|| {
                issue_workspace_error(
                    IssueWorkspaceErrorCodeV1::NotFoundInProject,
                    Some(request.issue_id),
                    None,
                    None,
                )
            })?;
        if prior.row_version != request.expected_row_version {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::StaleVersion,
                Some(request.issue_id),
                Some(request.expected_row_version),
                Some(prior.row_version),
            ));
        }
        if prior.archived_at.is_some() {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidTransition,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        if let Err(error) =
            rsi_common::types::validate_agent_issue_status_transition(prior.status, request.status)
        {
            let code = match error {
                IssueStatusTransitionErrorV1::NoSemanticChange => {
                    IssueWorkspaceErrorCodeV1::NoSemanticChange
                }
                IssueStatusTransitionErrorV1::InvalidTransition => {
                    IssueWorkspaceErrorCodeV1::InvalidTransition
                }
            };
            return Err(issue_workspace_error(
                code,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        let now = Utc::now();
        let mut next = prior.clone();
        next.status = request.status;
        next.closed_at = if request.status.is_terminal() {
            prior.closed_at.or(Some(now))
        } else {
            None
        };
        next.updated_at = now;
        next.row_version += 1;
        let result = Self::operator_issue_projection_cas_tx(
            &tx,
            &prior,
            &next,
            IssueEventOperationV1::StatusUpdated,
            semantic_request,
            &request.idempotency_key,
            "rsi-rpc:UpdateIssueStatusV2",
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(result)
    }

    fn issue_archive_state_workspace(
        &self,
        project_id: Uuid,
        issue_id: Uuid,
        expected_row_version: i64,
        idempotency_key: &str,
        restore: bool,
        semantic_request: IssueSemanticRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        let operation = if restore {
            IssueEventOperationV1::Restored
        } else {
            IssueEventOperationV1::Archived
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(replay) = Self::operator_replay_tx(
            &tx,
            project_id,
            issue_id,
            expected_row_version,
            idempotency_key,
            operation,
            &semantic_request,
        )? {
            issue_write_before_commit()?;
            tx.commit()?;
            return Ok(replay);
        }
        let prior = Self::get_issue_in_project_tx(&tx, project_id, issue_id)?.ok_or_else(|| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NotFoundInProject,
                Some(issue_id),
                None,
                None,
            )
        })?;
        if prior.row_version != expected_row_version {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::StaleVersion,
                Some(issue_id),
                Some(expected_row_version),
                Some(prior.row_version),
            ));
        }
        if restore && prior.archived_at.is_none() {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NotArchived,
                Some(issue_id),
                None,
                None,
            ));
        }
        if !restore && prior.archived_at.is_some() {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidTransition,
                Some(issue_id),
                None,
                None,
            ));
        }
        if !restore && !prior.status.is_terminal() {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::NotTerminal,
                Some(issue_id),
                None,
                None,
            ));
        }
        let now = Utc::now();
        let mut next = prior.clone();
        next.archived_at = if restore { None } else { Some(now) };
        next.updated_at = now;
        next.row_version += 1;
        let result = Self::operator_issue_projection_cas_tx(
            &tx,
            &prior,
            &next,
            operation,
            semantic_request,
            idempotency_key,
            if restore {
                "rsi-rpc:RestoreIssue"
            } else {
                "rsi-rpc:ArchiveIssue"
            },
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn archive_issue_workspace(
        &self,
        request: &ArchiveIssueRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        self.issue_archive_state_workspace(
            request.project_id,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            false,
            request.semantic_request(),
        )
    }

    pub(crate) fn restore_issue_workspace(
        &self,
        request: &RestoreIssueRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        self.issue_archive_state_workspace(
            request.project_id,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            true,
            request.semantic_request(),
        )
    }

    fn agent_replay_tx(
        tx: &Transaction<'_>,
        authority: AgentIssueAuthority,
        issue_id: Uuid,
        expected_row_version: i64,
        idempotency_key: &str,
        operation: IssueEventOperationV1,
        request: &IssueSemanticRequestV1,
    ) -> Result<Option<AgentIssueMutationResultV1>> {
        let Some(event) =
            Self::event_by_actor_key_tx(tx, authority.caller_session_id, idempotency_key)?
        else {
            return Ok(None);
        };
        let fingerprint = request.fingerprint().map_err(DaemonError::InvalidParam)?;
        if event.project_id != authority.project_id
            || event.issue_id != issue_id
            || event.operation != operation
            || event.expected_row_version != expected_row_version
            || event.request_fingerprint != fingerprint
        {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::IdempotencyConflict,
                None,
                None,
            ));
        }
        Ok(Some(AgentIssueMutationResultV1 {
            issue: event.issue.clone(),
            event,
            deduplicated: true,
        }))
    }

    /// Guarded Issue projection CAS plus its immutable receipt, dispatched on
    /// the resolved actor. The lead and manager paths each own their CAS; the
    /// receipt records the actor truthfully (a manager never carries an
    /// owning Epic).
    fn agent_issue_projection_cas_tx(
        tx: &Transaction<'_>,
        authority: AgentIssueAuthority,
        prior: &Issue,
        next: &Issue,
        operation: IssueEventOperationV1,
        request: IssueSemanticRequestV1,
        idempotency_key: &str,
    ) -> Result<AgentIssueMutationResultV1> {
        let actor = match authority.actor {
            AgentIssueActor::Lead {
                epic_id,
                lead_generation,
            } => {
                Self::agent_issue_lead_projection_cas_tx(
                    tx,
                    authority,
                    epic_id,
                    lead_generation,
                    prior,
                    next,
                    operation,
                )?;
                IssueWriteActor::Session {
                    session_id: authority.caller_session_id,
                    owning_epic_id: Some(epic_id),
                    idempotency_key: idempotency_key.to_string(),
                }
            }
            AgentIssueActor::Manager { scope_version } => {
                Self::agent_issue_manager_projection_cas_tx(
                    tx,
                    authority,
                    scope_version,
                    prior,
                    next,
                    operation,
                )?;
                IssueWriteActor::Manager {
                    session_id: authority.caller_session_id,
                    idempotency_key: idempotency_key.to_string(),
                }
            }
        };
        issue_projection_written()?;
        let event = build_issue_event(
            next.clone(),
            next.row_version,
            operation,
            &actor,
            prior.row_version,
            request,
            next.updated_at,
        )?;
        insert_issue_event_tx(tx, &event)?;
        Ok(AgentIssueMutationResultV1 {
            issue: next.clone(),
            event,
            deduplicated: false,
        })
    }

    /// Owning-Epic lead CAS: the row-version UPDATE is itself fenced on the
    /// resolved Epic lead pointer and lead generation.
    fn agent_issue_lead_projection_cas_tx(
        tx: &Transaction<'_>,
        authority: AgentIssueAuthority,
        epic_id: Uuid,
        lead_generation: i64,
        prior: &Issue,
        next: &Issue,
        operation: IssueEventOperationV1,
    ) -> Result<()> {
        let changed = match operation {
            IssueEventOperationV1::ContentUpdated => tx.execute(
                "UPDATE issues
                 SET title=?1, body=?2, labels=?3, priority=?4, assignee=?5,
                     updated_at=?6, row_version=row_version+1
                 WHERE id=?7 AND project_id=?8 AND row_version=?9
                   AND EXISTS(
                     SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
                     WHERE e.id=?10 AND e.session_kind='Epic' AND e.lead_session_id=?11
                       AND e.project_id=?12 AND g.generation=?13
                   )",
                params![
                    &next.title,
                    &next.body,
                    serde_json::to_string(&next.labels)?,
                    next.priority,
                    next.assignee.as_deref(),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    authority.project_id.to_string(),
                    prior.row_version,
                    epic_id.to_string(),
                    authority.caller_session_id.to_string(),
                    authority.project_id.to_string(),
                    lead_generation,
                ],
            )?,
            IssueEventOperationV1::StatusUpdated => tx.execute(
                "UPDATE issues
                 SET status=?1, closed_at=?2, updated_at=?3, row_version=row_version+1
                 WHERE id=?4 AND project_id=?5 AND row_version=?6
                   AND EXISTS(
                     SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
                     WHERE e.id=?7 AND e.session_kind='Epic' AND e.lead_session_id=?8
                       AND e.project_id=?9 AND g.generation=?10
                   )",
                params![
                    next.status.as_str(),
                    next.closed_at.map(canonical_timestamp),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    authority.project_id.to_string(),
                    prior.row_version,
                    epic_id.to_string(),
                    authority.caller_session_id.to_string(),
                    authority.project_id.to_string(),
                    lead_generation,
                ],
            )?,
            IssueEventOperationV1::Archived | IssueEventOperationV1::Restored => tx.execute(
                "UPDATE issues
                 SET archived_at=?1, updated_at=?2, row_version=row_version+1
                 WHERE id=?3 AND project_id=?4 AND row_version=?5
                   AND EXISTS(
                     SELECT 1 FROM sessions e JOIN epic_lead_generations g ON g.epic_id=e.id
                     WHERE e.id=?6 AND e.session_kind='Epic' AND e.lead_session_id=?7
                       AND e.project_id=?8 AND g.generation=?9
                   )",
                params![
                    next.archived_at.map(canonical_timestamp),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    authority.project_id.to_string(),
                    prior.row_version,
                    epic_id.to_string(),
                    authority.caller_session_id.to_string(),
                    authority.project_id.to_string(),
                    lead_generation,
                ],
            )?,
            _ => {
                return Err(DaemonError::Store(
                    "unsupported guarded Issue projection operation".to_string(),
                ));
            }
        };
        if changed != 1 {
            let still_authorized =
                Self::resolve_agent_issue_authority_tx(tx, authority.caller_session_id)
                    .ok()
                    .is_some_and(|current| {
                        current.project_id == authority.project_id
                            && current.actor == authority.actor
                    });
            if !still_authorized {
                return Err(agent_issue_error(
                    rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                    None,
                    None,
                ));
            }
            let actual = Self::get_issue_in_project_tx(tx, authority.project_id, prior.id)?
                .map(|issue| issue.row_version);
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::StaleVersion,
                Some(prior.row_version),
                actual,
            ));
        }
        Ok(())
    }

    /// Manager CAS: re-resolve the `IssueCoordinate` grant against the live
    /// appointment scope inside this immediate transaction (which holds the
    /// write lock through commit), then apply a plain row-version UPDATE. There
    /// is no lead-generation fence because a manager has no owning Epic.
    fn agent_issue_manager_projection_cas_tx(
        tx: &Transaction<'_>,
        authority: AgentIssueAuthority,
        scope_version: i64,
        prior: &Issue,
        next: &Issue,
        operation: IssueEventOperationV1,
    ) -> Result<()> {
        let still_authorized =
            Self::resolve_agent_issue_coordinator_authority_tx(tx, authority.caller_session_id)
                .ok()
                .is_some_and(|current| {
                    current.project_id == authority.project_id
                        && current.actor == AgentIssueActor::Manager { scope_version }
                });
        if !still_authorized {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            ));
        }
        let changed = match operation {
            IssueEventOperationV1::ContentUpdated => tx.execute(
                "UPDATE issues
                 SET title=?1, body=?2, labels=?3, priority=?4, assignee=?5,
                     updated_at=?6, row_version=row_version+1
                 WHERE id=?7 AND project_id=?8 AND row_version=?9",
                params![
                    &next.title,
                    &next.body,
                    serde_json::to_string(&next.labels)?,
                    next.priority,
                    next.assignee.as_deref(),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    authority.project_id.to_string(),
                    prior.row_version,
                ],
            )?,
            IssueEventOperationV1::StatusUpdated => tx.execute(
                "UPDATE issues
                 SET status=?1, closed_at=?2, updated_at=?3, row_version=row_version+1
                 WHERE id=?4 AND project_id=?5 AND row_version=?6",
                params![
                    next.status.as_str(),
                    next.closed_at.map(canonical_timestamp),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    authority.project_id.to_string(),
                    prior.row_version,
                ],
            )?,
            IssueEventOperationV1::Archived | IssueEventOperationV1::Restored => tx.execute(
                "UPDATE issues
                 SET archived_at=?1, updated_at=?2, row_version=row_version+1
                 WHERE id=?3 AND project_id=?4 AND row_version=?5",
                params![
                    next.archived_at.map(canonical_timestamp),
                    canonical_timestamp(next.updated_at),
                    prior.id.to_string(),
                    authority.project_id.to_string(),
                    prior.row_version,
                ],
            )?,
            _ => {
                return Err(DaemonError::Store(
                    "unsupported guarded Issue projection operation".to_string(),
                ));
            }
        };
        if changed != 1 {
            let actual = Self::get_issue_in_project_tx(tx, authority.project_id, prior.id)?
                .map(|issue| issue.row_version);
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::StaleVersion,
                Some(prior.row_version),
                actual,
            ));
        }
        Ok(())
    }

    /// Bounded, project-scoped Issue list for the authenticated owning-Epic
    /// lead or approved manager coordinator. The transaction covers persisted
    /// topology/policy resolution and the page snapshot so a caller cannot
    /// inject a project scope through JSON.
    pub(crate) fn agent_list_issues(
        &self,
        caller_session_id: Uuid,
        request: &AgentListIssuesRequestV1,
    ) -> Result<IssuePageV1> {
        let limit = request.validated_limit().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = Self::resolve_agent_issue_authority(&tx, caller_session_id)?;
        let status = request.status.map(|status| status.as_str().to_string());
        let archive_filter = match request.archive {
            IssueArchiveFilterV1::Active => 0,
            IssueArchiveFilterV1::Archived => 1,
            IssueArchiveFilterV1::All => 2,
        };
        let cursor_display_number = request.cursor.as_ref().map(|cursor| cursor.display_number);
        let cursor_issue_id = request
            .cursor
            .as_ref()
            .map(|cursor| cursor.issue_id.to_string());
        let ready_filter = if request.ready {
            " AND i.status='Open' AND i.archived_at IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM issue_deps d
                  JOIN issues b ON b.id=d.depends_on_id AND b.project_id=d.project_id
                  WHERE d.project_id=i.project_id AND d.issue_id=i.id
                    AND b.status IN ('Open','InProgress'))"
        } else {
            ""
        };
        let rows = tx
            .prepare(&format!(
                "SELECT {QUALIFIED_ISSUE_COLUMNS} FROM issues i
                 WHERE i.project_id=?1
                   AND (?2 IS NULL OR i.status=?2)
                   AND (?3=2 OR (?3=0 AND i.archived_at IS NULL) OR (?3=1 AND i.archived_at IS NOT NULL))
                   AND (?4 IS NULL OR i.display_number>?4 OR (i.display_number=?4 AND i.id>?5))
                   {ready_filter}
                 ORDER BY i.display_number ASC,i.id ASC LIMIT ?6"
            ))?
            .query_map(
                params![
                    authority.project_id.to_string(),
                    status,
                    archive_filter,
                    cursor_display_number,
                    cursor_issue_id,
                    i64::from(limit + 1),
                ],
                map_issue_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut issues = rows
            .into_iter()
            .map(IssueRow::into_issue)
            .collect::<Result<Vec<_>>>()?;
        let next_cursor = if issues.len() > limit as usize {
            let next = issues.pop().expect("checked page overflow");
            let tail = issues.last().expect("limit is at least one");
            let _ = next;
            Some(IssueListCursorV1 {
                display_number: tail.display_number,
                issue_id: tail.id,
            })
        } else {
            None
        };
        tx.commit()?;
        Ok(IssuePageV1 {
            issues,
            next_cursor,
        })
    }

    pub(crate) fn agent_get_issue(
        &self,
        caller_session_id: Uuid,
        request: &AgentGetIssueRequestV1,
    ) -> Result<AgentGetIssueResultV1> {
        request.validate().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = Self::resolve_agent_issue_authority(&tx, caller_session_id)?;
        let issue = Self::get_issue_in_project_tx(&tx, authority.project_id, request.issue_id)?
            .ok_or_else(|| {
                agent_issue_error(
                    rsi_common::rpc::AgentIssueErrorCodeV1::NotFoundInScope,
                    None,
                    None,
                )
            })?;
        // Each statement is prepared from a literal so the protected-DML
        // writer audit can prove these reads static (#392).
        let load_related = |mut statement: rusqlite::Statement<'_>| -> Result<(
            Vec<AgentIssueDependencyRefV1>,
            bool,
        )> {
            let mut rows = statement
                .query_map(
                    params![
                        authority.project_id.to_string(),
                        request.issue_id.to_string(),
                        AGENT_ISSUE_DEPENDENCY_FETCH_LIMIT
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let truncated = rows.len() > AGENT_ISSUE_DEPENDENCY_LIMIT;
            rows.truncate(AGENT_ISSUE_DEPENDENCY_LIMIT);
            let related = rows
                .into_iter()
                .map(|(id, display_number, status)| {
                    Ok(AgentIssueDependencyRefV1 {
                        id: Uuid::parse_str(&id).map_err(|error| {
                            DaemonError::Store(format!("Invalid related Issue id: {error}"))
                        })?,
                        display_number,
                        status: IssueStatus::parse(&status).map_err(DaemonError::Store)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((related, truncated))
        };
        let (blocked_by, blocked_by_truncated) = load_related(tx.prepare(
            "SELECT i.id,i.display_number,i.status FROM issue_deps d
             JOIN issues i ON i.id=d.depends_on_id AND i.project_id=d.project_id
             WHERE d.project_id=?1 AND d.issue_id=?2
             ORDER BY i.display_number ASC,i.id ASC LIMIT ?3",
        )?)?;
        let (blocks, blocks_truncated) = load_related(tx.prepare(
            "SELECT i.id,i.display_number,i.status FROM issue_deps d
             JOIN issues i ON i.id=d.issue_id AND i.project_id=d.project_id
             WHERE d.project_id=?1 AND d.depends_on_id=?2
             ORDER BY i.display_number ASC,i.id ASC LIMIT ?3",
        )?)?;
        tx.commit()?;
        Ok(AgentGetIssueResultV1 {
            issue,
            blocked_by,
            blocked_by_truncated,
            blocks,
            blocks_truncated,
        })
    }

    pub(crate) fn agent_list_issue_events(
        &self,
        caller_session_id: Uuid,
        request: &IssueEventPageRequestV1,
    ) -> Result<IssueEventPageV1> {
        let limit = request.validated_limit().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = Self::resolve_agent_issue_authority(&tx, caller_session_id)?;
        if Self::get_issue_in_project_tx(&tx, authority.project_id, request.issue_id)?.is_none() {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::NotFoundInScope,
                None,
                None,
            ));
        }
        let rows = tx
            .prepare(&format!(
                "SELECT {ISSUE_EVENT_COLUMNS} FROM issue_events
                 WHERE project_id=?1 AND issue_id=?2 AND sequence>?3
                 ORDER BY sequence ASC LIMIT ?4"
            ))?
            .query_map(
                params![
                    authority.project_id.to_string(),
                    request.issue_id.to_string(),
                    request.after_sequence,
                    i64::from(limit + 1)
                ],
                issue_event_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut events = rows;
        let next_after_sequence = if events.len() > limit as usize {
            events.pop();
            events.last().map(|event| event.sequence)
        } else {
            None
        };
        tx.commit()?;
        Ok(IssueEventPageV1 {
            events,
            next_after_sequence,
        })
    }

    /// Operator-only history reader. This is deliberately bounded even though
    /// legacy operator Issue lists remain unbounded for compatibility.
    pub(crate) fn list_issue_events_v1(
        &self,
        request: &IssueEventPageRequestV1,
    ) -> Result<IssueEventPageV1> {
        let limit = request
            .validated_limit()
            .map_err(DaemonError::InvalidParam)?;
        let issue = self
            .get_issue(request.issue_id)?
            .ok_or_else(|| DaemonError::Store("Issue not found".to_string()))?;
        let mut statement = self.conn.prepare(&format!(
            "SELECT {ISSUE_EVENT_COLUMNS} FROM issue_events
             WHERE project_id=?1 AND issue_id=?2 AND sequence>?3
             ORDER BY sequence ASC LIMIT ?4"
        ))?;
        let mut events = statement
            .query_map(
                params![
                    issue.project_id.to_string(),
                    issue.id.to_string(),
                    request.after_sequence,
                    i64::from(limit + 1)
                ],
                issue_event_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next_after_sequence = if events.len() > limit as usize {
            events.pop();
            events.last().map(|event| event.sequence)
        } else {
            None
        };
        Ok(IssueEventPageV1 {
            events,
            next_after_sequence,
        })
    }

    pub(crate) fn agent_update_issue(
        &self,
        caller_session_id: Uuid,
        request: &AgentUpdateIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        request.validate().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = Self::resolve_agent_issue_authority(&tx, caller_session_id)?;
        let semantic_request = request.semantic_request().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        if let Some(replay) = Self::agent_replay_tx(
            &tx,
            authority,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            IssueEventOperationV1::ContentUpdated,
            &semantic_request,
        )? {
            issue_write_before_commit()?;
            tx.commit()?;
            return Ok(replay);
        }
        let prior = Self::get_issue_in_project_tx(&tx, authority.project_id, request.issue_id)?
            .ok_or_else(|| {
                agent_issue_error(
                    rsi_common::rpc::AgentIssueErrorCodeV1::NotFoundInScope,
                    None,
                    None,
                )
            })?;
        if prior.row_version != request.expected_row_version {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::StaleVersion,
                Some(request.expected_row_version),
                Some(prior.row_version),
            ));
        }
        if prior.archived_at.is_some() {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::Archived,
                None,
                None,
            ));
        }
        if !matches!(prior.status, IssueStatus::Open | IssueStatus::InProgress) {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidTransition,
                None,
                None,
            ));
        }
        let title = request.title.clone().unwrap_or_else(|| prior.title.clone());
        let body = request.body.clone().unwrap_or_else(|| prior.body.clone());
        let labels = request
            .labels
            .clone()
            .unwrap_or_else(|| prior.labels.clone());
        let priority = if request.clear_priority {
            None
        } else {
            request.priority.or(prior.priority)
        };
        let assignee = if request.clear_assignee {
            None
        } else {
            request.assignee.clone().or_else(|| prior.assignee.clone())
        };
        if title == prior.title
            && body == prior.body
            && labels == prior.labels
            && priority == prior.priority
            && assignee == prior.assignee
        {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::NoSemanticChange,
                None,
                None,
            ));
        }
        let now = Utc::now();
        let mut next = prior.clone();
        next.title = title.clone();
        next.body = body.clone();
        next.labels = labels.clone();
        next.priority = priority;
        next.assignee = assignee.clone();
        next.updated_at = now;
        next.row_version += 1;
        let result = Self::agent_issue_projection_cas_tx(
            &tx,
            authority,
            &prior,
            &next,
            IssueEventOperationV1::ContentUpdated,
            semantic_request,
            &request.idempotency_key,
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn agent_update_issue_status(
        &self,
        caller_session_id: Uuid,
        request: &AgentUpdateIssueStatusRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        request.validate().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = Self::resolve_agent_issue_authority(&tx, caller_session_id)?;
        let semantic_request = request.semantic_request().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        if let Some(replay) = Self::agent_replay_tx(
            &tx,
            authority,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            IssueEventOperationV1::StatusUpdated,
            &semantic_request,
        )? {
            issue_write_before_commit()?;
            tx.commit()?;
            return Ok(replay);
        }
        let prior = Self::get_issue_in_project_tx(&tx, authority.project_id, request.issue_id)?
            .ok_or_else(|| {
                agent_issue_error(
                    rsi_common::rpc::AgentIssueErrorCodeV1::NotFoundInScope,
                    None,
                    None,
                )
            })?;
        if prior.row_version != request.expected_row_version {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::StaleVersion,
                Some(request.expected_row_version),
                Some(prior.row_version),
            ));
        }
        if prior.archived_at.is_some() {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::Archived,
                None,
                None,
            ));
        }
        if let Err(error) =
            rsi_common::types::validate_agent_issue_status_transition(prior.status, request.status)
        {
            let code = match error {
                IssueStatusTransitionErrorV1::NoSemanticChange => {
                    rsi_common::rpc::AgentIssueErrorCodeV1::NoSemanticChange
                }
                IssueStatusTransitionErrorV1::InvalidTransition => {
                    rsi_common::rpc::AgentIssueErrorCodeV1::InvalidTransition
                }
            };
            return Err(agent_issue_error(code, None, None));
        }
        let now = Utc::now();
        let closed_at = if request.status.is_terminal() {
            prior.closed_at.or(Some(now))
        } else {
            None
        };
        let mut next = prior.clone();
        next.status = request.status;
        next.closed_at = closed_at;
        next.updated_at = now;
        next.row_version += 1;
        let result = Self::agent_issue_projection_cas_tx(
            &tx,
            authority,
            &prior,
            &next,
            IssueEventOperationV1::StatusUpdated,
            semantic_request,
            &request.idempotency_key,
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(result)
    }

    fn agent_archive_state_change(
        &self,
        caller_session_id: Uuid,
        issue_id: Uuid,
        expected_row_version: i64,
        idempotency_key: &str,
        restore: bool,
    ) -> Result<AgentIssueMutationResultV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = Self::resolve_agent_issue_authority(&tx, caller_session_id)?;
        let operation = if restore {
            IssueEventOperationV1::Restored
        } else {
            IssueEventOperationV1::Archived
        };
        let semantic_request = if restore {
            IssueSemanticRequestV1::new(IssueSemanticOperationV1::Restored {
                issue_id,
                expected_row_version,
            })
        } else {
            IssueSemanticRequestV1::new(IssueSemanticOperationV1::Archived {
                issue_id,
                expected_row_version,
            })
        };
        if let Some(replay) = Self::agent_replay_tx(
            &tx,
            authority,
            issue_id,
            expected_row_version,
            idempotency_key,
            operation,
            &semantic_request,
        )? {
            issue_write_before_commit()?;
            tx.commit()?;
            return Ok(replay);
        }
        let prior = Self::get_issue_in_project_tx(&tx, authority.project_id, issue_id)?
            .ok_or_else(|| {
                agent_issue_error(
                    rsi_common::rpc::AgentIssueErrorCodeV1::NotFoundInScope,
                    None,
                    None,
                )
            })?;
        if prior.row_version != expected_row_version {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::StaleVersion,
                Some(expected_row_version),
                Some(prior.row_version),
            ));
        }
        if restore && prior.archived_at.is_none() {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::NotArchived,
                None,
                None,
            ));
        }
        if !restore && prior.archived_at.is_some() {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::Archived,
                None,
                None,
            ));
        }
        if !restore && !prior.status.is_terminal() {
            return Err(agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidTransition,
                None,
                None,
            ));
        }
        let now = Utc::now();
        let mut next = prior.clone();
        next.archived_at = if restore { None } else { Some(now) };
        next.updated_at = now;
        next.row_version += 1;
        let result = Self::agent_issue_projection_cas_tx(
            &tx,
            authority,
            &prior,
            &next,
            operation,
            semantic_request,
            idempotency_key,
        )?;
        issue_write_before_commit()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn agent_archive_issue(
        &self,
        caller_session_id: Uuid,
        request: &AgentArchiveIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        request.validate().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        self.agent_archive_state_change(
            caller_session_id,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            false,
        )
    }

    pub(crate) fn agent_restore_issue(
        &self,
        caller_session_id: Uuid,
        request: &AgentRestoreIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        request.validate().map_err(|_| {
            agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
                None,
                None,
            )
        })?;
        self.agent_archive_state_change(
            caller_session_id,
            request.issue_id,
            request.expected_row_version,
            &request.idempotency_key,
            true,
        )
    }

    /// Project-bound lifecycle transition used by local automation.
    pub fn update_issue_status_in_project(
        &self,
        project_id: Uuid,
        id: Uuid,
        status: IssueStatus,
    ) -> Result<Issue> {
        self.update_issue_status_scoped(
            Some(project_id),
            id,
            status,
            IssueWriteActor::System {
                label: "rsi:local-issue-tracker".to_string(),
            },
        )
    }

    fn dependency_endpoints_in_project_tx(
        tx: &Transaction<'_>,
        project_id: Uuid,
        issue_id: Uuid,
        depends_on_id: Uuid,
    ) -> Result<bool> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM issues
             WHERE project_id=?1 AND id IN (?2, ?3)",
            params![
                project_id.to_string(),
                issue_id.to_string(),
                depends_on_id.to_string()
            ],
            |row| row.get(0),
        )?;
        Ok(count == 2)
    }

    fn issue_dep_creates_cycle_tx(
        tx: &Transaction<'_>,
        project_id: Uuid,
        issue_id: Uuid,
        depends_on_id: Uuid,
    ) -> Result<bool> {
        let creates_cycle: bool = tx.query_row(
            "WITH RECURSIVE reach(id, depth) AS (
                 SELECT ?1, 0
                 UNION
                 SELECT d.depends_on_id, reach.depth + 1
                 FROM issue_deps d JOIN reach ON d.issue_id = reach.id
                 WHERE d.project_id = ?3 AND reach.depth < ?4
             )
             SELECT EXISTS(SELECT 1 FROM reach WHERE id = ?2)",
            params![
                depends_on_id.to_string(),
                issue_id.to_string(),
                project_id.to_string(),
                DEP_CYCLE_MAX_DEPTH
            ],
            |row| row.get(0),
        )?;
        Ok(creates_cycle)
    }

    fn insert_issue_dep_tx(
        tx: &Transaction<'_>,
        project_id: Uuid,
        issue_id: Uuid,
        depends_on_id: Uuid,
    ) -> Result<bool> {
        let changed = tx.execute(
            "INSERT OR IGNORE INTO issue_deps (project_id, issue_id, depends_on_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                project_id.to_string(),
                issue_id.to_string(),
                depends_on_id.to_string(),
                now_nanos()
            ],
        )?;
        Ok(changed > 0)
    }

    pub(crate) fn add_issue_dependency_workspace(
        &self,
        request: &IssueDependencyMutationRequestV1,
    ) -> Result<IssueDependencyMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if !Self::dependency_endpoints_in_project_tx(
            &tx,
            request.project_id,
            request.issue_id,
            request.depends_on_id,
        )? {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::DependencyEndpoint,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        if Self::issue_dep_creates_cycle_tx(
            &tx,
            request.project_id,
            request.issue_id,
            request.depends_on_id,
        )? {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::DependencyCycle,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        let changed = Self::insert_issue_dep_tx(
            &tx,
            request.project_id,
            request.issue_id,
            request.depends_on_id,
        )?;
        tx.commit()?;
        Ok(IssueDependencyMutationResultV1 {
            issue_id: request.issue_id,
            depends_on_id: request.depends_on_id,
            changed,
        })
    }

    pub(crate) fn remove_issue_dependency_workspace(
        &self,
        request: &IssueDependencyMutationRequestV1,
    ) -> Result<IssueDependencyMutationResultV1> {
        request.validate().map_err(|_| {
            issue_workspace_error(
                IssueWorkspaceErrorCodeV1::InvalidRequest,
                Some(request.issue_id),
                None,
                None,
            )
        })?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if !Self::dependency_endpoints_in_project_tx(
            &tx,
            request.project_id,
            request.issue_id,
            request.depends_on_id,
        )? {
            return Err(issue_workspace_error(
                IssueWorkspaceErrorCodeV1::DependencyEndpoint,
                Some(request.issue_id),
                None,
                None,
            ));
        }
        let changed = tx.execute(
            "DELETE FROM issue_deps WHERE project_id=?1 AND issue_id=?2 AND depends_on_id=?3",
            params![
                request.project_id.to_string(),
                request.issue_id.to_string(),
                request.depends_on_id.to_string()
            ],
        )? > 0;
        tx.commit()?;
        Ok(IssueDependencyMutationResultV1 {
            issue_id: request.issue_id,
            depends_on_id: request.depends_on_id,
            changed,
        })
    }

    /// Add a dependency edge: `issue_id` depends on (is blocked by)
    /// `depends_on_id`. Self-deps are rejected with a clean error; cycles are
    /// rejected at write time via a depth-capped recursive walk of the
    /// blocker's own `depends_on` edges (so the ready-work query never needs
    /// cycle logic). A duplicate edge is an idempotent `Ok(())`.
    pub fn add_issue_dep(&self, issue_id: Uuid, depends_on_id: Uuid) -> Result<()> {
        if issue_id == depends_on_id {
            return Err(DaemonError::Store(format!(
                "Issue {} cannot depend on itself",
                issue_id
            )));
        }

        let tx = self.conn.unchecked_transaction()?;

        let issue_project: Option<String> = tx
            .query_row(
                "SELECT project_id FROM issues WHERE id = ?1",
                params![issue_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let blocker_project: Option<String> = tx
            .query_row(
                "SELECT project_id FROM issues WHERE id = ?1",
                params![depends_on_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(project_id) = issue_project else {
            return Err(DaemonError::Store(format!("Issue not found: {issue_id}")));
        };
        if blocker_project.as_deref() != Some(project_id.as_str()) {
            return Err(DaemonError::Store(
                "Issue dependency endpoints must share a project".to_string(),
            ));
        }

        let project_id = Uuid::parse_str(&project_id)
            .map_err(|_| DaemonError::Store("Invalid dependency project UUID".to_string()))?;
        if Self::issue_dep_creates_cycle_tx(&tx, project_id, issue_id, depends_on_id)? {
            return Err(DaemonError::Store(format!(
                "Dependency cycle: {} already (transitively) depends on {}",
                depends_on_id, issue_id
            )));
        }

        // INSERT OR IGNORE makes duplicates idempotent; FK violations
        // (unknown endpoint) are NOT ignored and still surface as errors.
        // The self-dep CHECK never fires here (rejected in Rust above).
        Self::insert_issue_dep_tx(&tx, project_id, issue_id, depends_on_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove a dependency edge. Returns `true` if an edge was deleted.
    pub fn remove_issue_dep(&self, issue_id: Uuid, depends_on_id: Uuid) -> Result<bool> {
        let Some(project_id) = self.get_issue(issue_id)?.map(|issue| issue.project_id) else {
            return Err(DaemonError::Store(format!("Issue not found: {issue_id}")));
        };
        let n = self.conn.execute(
            "DELETE FROM issue_deps WHERE project_id = ?1 AND issue_id = ?2 AND depends_on_id = ?3",
            params![
                project_id.to_string(),
                issue_id.to_string(),
                depends_on_id.to_string()
            ],
        )?;
        Ok(n > 0)
    }

    /// Edges where `issue_id` is the dependent — i.e. its blockers.
    pub fn list_issue_deps(&self, issue_id: Uuid) -> Result<Vec<IssueDep>> {
        let Some(project_id) = self.get_issue(issue_id)?.map(|issue| issue.project_id) else {
            return Err(DaemonError::Store(format!("Issue not found: {issue_id}")));
        };
        let mut stmt = self.conn.prepare(
            "SELECT project_id, issue_id, depends_on_id, created_at FROM issue_deps
             WHERE project_id = ?1 AND issue_id = ?2 ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map(
                params![project_id.to_string(), issue_id.to_string()],
                map_dep_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(dep_from_row).collect()
    }

    /// Edges where `issue_id` is the blocker — i.e. the issues it blocks
    /// (dependents; feeds unblock events later).
    pub fn list_issue_dependents(&self, issue_id: Uuid) -> Result<Vec<IssueDep>> {
        let Some(project_id) = self.get_issue(issue_id)?.map(|issue| issue.project_id) else {
            return Err(DaemonError::Store(format!("Issue not found: {issue_id}")));
        };
        let mut stmt = self.conn.prepare(
            "SELECT project_id, issue_id, depends_on_id, created_at FROM issue_deps
             WHERE project_id = ?1 AND depends_on_id = ?2 ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map(
                params![project_id.to_string(), issue_id.to_string()],
                map_dep_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(dep_from_row).collect()
    }

    /// Ready-work query: `Open` issues with no blocker in a non-terminal
    /// status (`Open`/`InProgress`). `InProgress` issues are never ready
    /// (already claimed). Ordered priority-then-age with NULL priority last
    /// (beads "hybrid" sort). `None` limit = unbounded (SQLite `LIMIT -1`).
    pub fn list_ready_issues(
        &self,
        project_id: Option<Uuid>,
        limit: Option<u32>,
    ) -> Result<Vec<Issue>> {
        let (sql, params): (String, Vec<rusqlite::types::Value>) =
            if let Some(project_id) = project_id {
                (
                    format!(
                        "SELECT {ISSUE_COLUMNS} FROM issues i
                     WHERE i.project_id = ?1 AND i.status = 'Open' AND i.archived_at IS NULL
                       AND NOT EXISTS (
                         SELECT 1 FROM issue_deps d
                         JOIN issues b ON b.id = d.depends_on_id AND b.project_id = d.project_id
                         WHERE d.project_id = i.project_id AND d.issue_id = i.id
                           AND b.status IN ('Open','InProgress'))
                     ORDER BY (i.priority IS NULL), i.priority ASC, i.created_at ASC, i.id ASC
                     LIMIT ?2"
                    ),
                    vec![
                        project_id.to_string().into(),
                        limit.map(i64::from).unwrap_or(-1).into(),
                    ],
                )
            } else {
                (
                    format!(
                        "SELECT {ISSUE_COLUMNS} FROM issues i
                     WHERE i.status = 'Open' AND i.archived_at IS NULL
                       AND NOT EXISTS (
                         SELECT 1 FROM issue_deps d
                         JOIN issues b ON b.id = d.depends_on_id AND b.project_id = d.project_id
                         WHERE d.project_id = i.project_id AND d.issue_id = i.id
                           AND b.status IN ('Open','InProgress'))
                     ORDER BY (i.priority IS NULL), i.priority ASC, i.created_at ASC, i.id ASC
                     LIMIT ?1"
                    ),
                    vec![limit.map(i64::from).unwrap_or(-1).into()],
                )
            };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), map_issue_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|row| row.into_issue()).collect()
    }
}
