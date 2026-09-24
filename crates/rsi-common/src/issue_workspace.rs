//! Typed operator contracts for the first-class Issue workspace.
//!
//! These contracts are additive to the legacy local-Issue RPC surface. They
//! keep workspace reads project-scoped and bounded, and give operator writes
//! explicit optimistic-concurrency and durable retry identities.

use crate::types::{
    Issue, IssueArchiveFilterV1, IssueContentPatchV1, IssueDep, IssueEventPageV1, IssueEventV1,
    IssueSemanticOperationV1, IssueSemanticRequestV1, IssueStatus,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use uuid::Uuid;

pub const ISSUE_WORKSPACE_DEFAULT_LIMIT: u16 = 64;
pub const ISSUE_WORKSPACE_MAX_LIMIT: u16 = 256;
pub const OPERATOR_ISSUE_CREATE_DOMAIN_V1: &str = "rsi.issue.operator-create/v1";
pub const OPERATOR_ISSUE_MUTATION_DOMAIN_V1: &str = "rsi.issue.operator-mutation/v1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceReadinessFilterV1 {
    #[default]
    Any,
    Ready,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceReadinessV1 {
    Ready,
    Blocked,
    NotEligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceProvenanceV1 {
    Operator,
    Session,
    Idea,
    Finding,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceSortV1 {
    #[default]
    UpdatedDesc,
    DisplayNumberAsc,
    PriorityAsc,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageDirectionV1 {
    #[default]
    Forward,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IssueWorkspaceCursorV1 {
    UpdatedDesc {
        updated_at: DateTime<Utc>,
        display_number: i64,
        issue_id: Uuid,
    },
    DisplayNumberAsc {
        display_number: i64,
        issue_id: Uuid,
    },
    PriorityAsc {
        priority: Option<u8>,
        display_number: i64,
        issue_id: Uuid,
    },
}

impl IssueWorkspaceCursorV1 {
    fn validate(&self) -> Result<(), String> {
        let (display_number, issue_id) = match self {
            Self::UpdatedDesc {
                display_number,
                issue_id,
                ..
            }
            | Self::DisplayNumberAsc {
                display_number,
                issue_id,
            }
            | Self::PriorityAsc {
                display_number,
                issue_id,
                ..
            } => (*display_number, *issue_id),
        };
        if display_number < 1 || issue_id.is_nil() {
            return Err("Issue workspace cursor is invalid".to_string());
        }
        if let Self::PriorityAsc {
            priority: Some(priority),
            ..
        } = self
            && !(1..=4).contains(priority)
        {
            return Err("Issue workspace cursor priority must be 1..=4".to_string());
        }
        Ok(())
    }

    #[must_use]
    pub const fn matches_sort(&self, sort: IssueWorkspaceSortV1) -> bool {
        matches!(
            (self, sort),
            (Self::UpdatedDesc { .. }, IssueWorkspaceSortV1::UpdatedDesc)
                | (
                    Self::DisplayNumberAsc { .. },
                    IssueWorkspaceSortV1::DisplayNumberAsc
                )
                | (Self::PriorityAsc { .. }, IssueWorkspaceSortV1::PriorityAsc)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListIssuesPageRequestV1 {
    pub project_id: Uuid,
    #[serde(default)]
    pub statuses: Vec<IssueStatus>,
    #[serde(default)]
    pub priorities: Vec<u8>,
    #[serde(default)]
    pub readiness: IssueWorkspaceReadinessFilterV1,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub unassigned: bool,
    #[serde(default)]
    pub labels_all: Vec<String>,
    #[serde(default)]
    pub provenance: Vec<IssueWorkspaceProvenanceV1>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub archive: IssueArchiveFilterV1,
    #[serde(default)]
    pub sort: IssueWorkspaceSortV1,
    #[serde(default)]
    pub direction: PageDirectionV1,
    #[serde(default)]
    pub cursor: Option<IssueWorkspaceCursorV1>,
    #[serde(default = "default_workspace_limit")]
    pub limit: u16,
}

impl ListIssuesPageRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_project_id(self.project_id)?;
        validate_limit(self.limit)?;
        if self.assignee.is_some() && self.unassigned {
            return Err("assignee and unassigned cannot both be set".to_string());
        }
        if let Some(assignee) = &self.assignee {
            validate_assignee(assignee)?;
        }
        if self.priorities.iter().any(|value| !(1..=4).contains(value)) {
            return Err("Issue priorities must be 1..=4".to_string());
        }
        validate_labels(&self.labels_all)?;
        if let Some(query) = &self.query
            && (query.trim().is_empty() || query.len() > 4096 || query.contains('\0'))
        {
            return Err("Issue query must be nonblank, NUL-free, and at most 4096 bytes".into());
        }
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
            if !cursor.matches_sort(self.sort) {
                return Err("Issue cursor kind must match sort".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWorkspaceRowV1 {
    pub issue: Issue,
    pub readiness: IssueWorkspaceReadinessV1,
    pub open_blocker_count: u32,
    pub dependent_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWorkspacePageV1 {
    pub rows: Vec<IssueWorkspaceRowV1>,
    #[serde(default)]
    pub previous_cursor: Option<IssueWorkspaceCursorV1>,
    #[serde(default)]
    pub next_cursor: Option<IssueWorkspaceCursorV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetIssueInProjectRequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
}

impl GetIssueInProjectRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_project_issue_ids(self.project_id, self.issue_id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NullablePatchV1<T> {
    #[default]
    Unchanged,
    Clear,
    Set(T),
}

impl<T> NullablePatchV1<T> {
    #[must_use]
    pub const fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }
}

impl<T: Serialize> Serialize for NullablePatchV1<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Unchanged | Self::Clear => serializer.serialize_none(),
            Self::Set(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for NullablePatchV1<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueContentPatchV2 {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_non_null"
    )]
    pub title: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_non_null"
    )]
    pub body: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_non_null"
    )]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "NullablePatchV1::is_unchanged")]
    pub priority: NullablePatchV1<u8>,
    #[serde(default, skip_serializing_if = "NullablePatchV1::is_unchanged")]
    pub assignee: NullablePatchV1<String>,
}

impl IssueContentPatchV2 {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.labels.is_none()
            && self.priority.is_unchanged()
            && self.assignee.is_unchanged()
    }

    pub fn validate(&self) -> Result<(), String> {
        self.as_semantic_patch().validate()
    }

    #[must_use]
    pub fn as_semantic_patch(&self) -> IssueContentPatchV1 {
        IssueContentPatchV1 {
            title: self.title.clone(),
            body: self.body.clone(),
            labels: self.labels.clone(),
            priority: match self.priority {
                NullablePatchV1::Set(value) => Some(value),
                NullablePatchV1::Unchanged | NullablePatchV1::Clear => None,
            },
            clear_priority: matches!(self.priority, NullablePatchV1::Clear),
            assignee: match &self.assignee {
                NullablePatchV1::Set(value) => Some(value.clone()),
                NullablePatchV1::Unchanged | NullablePatchV1::Clear => None,
            },
            clear_assignee: matches!(self.assignee, NullablePatchV1::Clear),
        }
    }
}

fn deserialize_present_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("field must not be null"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIssueV2RequestV1 {
    pub project_id: Uuid,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    pub idempotency_key: String,
}

impl CreateIssueV2RequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_project_id(self.project_id)?;
        validate_idempotency_key(&self.idempotency_key)?;
        IssueSemanticRequestV1::new(IssueSemanticOperationV1::Created {
            create: crate::types::IssueCreateSemanticV1 {
                project_id: self.project_id,
                issue_id: operator_issue_create_id(self.project_id, &self.idempotency_key),
                title: self.title.clone(),
                body: self.body.clone(),
                priority: self.priority,
                labels: self.labels.clone(),
                created_by_session_id: None,
                assignee: self.assignee.clone(),
            },
        })
        .validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueRequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
    pub patch: IssueContentPatchV2,
}

impl UpdateIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_mutation_identity(
            self.project_id,
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )?;
        self.patch.validate()
    }

    pub fn semantic_request(&self) -> Result<IssueSemanticRequestV1, String> {
        self.validate()?;
        Ok(IssueSemanticRequestV1::new(
            IssueSemanticOperationV1::ContentUpdated {
                issue_id: self.issue_id,
                expected_row_version: self.expected_row_version,
                patch: self.patch.as_semantic_patch(),
            },
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateIssueStatusV2RequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
    pub status: IssueStatus,
}

impl UpdateIssueStatusV2RequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_mutation_identity(
            self.project_id,
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )
    }

    pub fn semantic_request(&self) -> Result<IssueSemanticRequestV1, String> {
        self.validate()?;
        Ok(IssueSemanticRequestV1::new(
            IssueSemanticOperationV1::StatusUpdated {
                issue_id: self.issue_id,
                expected_row_version: self.expected_row_version,
                status: self.status,
            },
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveIssueRequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
}

impl ArchiveIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_mutation_identity(
            self.project_id,
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )
    }

    #[must_use]
    pub fn semantic_request(&self) -> IssueSemanticRequestV1 {
        IssueSemanticRequestV1::new(IssueSemanticOperationV1::Archived {
            issue_id: self.issue_id,
            expected_row_version: self.expected_row_version,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreIssueRequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
}

impl RestoreIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_mutation_identity(
            self.project_id,
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )
    }

    #[must_use]
    pub fn semantic_request(&self) -> IssueSemanticRequestV1 {
        IssueSemanticRequestV1::new(IssueSemanticOperationV1::Restored {
            issue_id: self.issue_id,
            expected_row_version: self.expected_row_version,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorIssueMutationResultV1 {
    pub issue: Issue,
    pub event: IssueEventV1,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueDependencyDirectionV1 {
    BlockedBy,
    Blocks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueDependencyCursorV1 {
    pub display_number: i64,
    pub issue_id: Uuid,
}

impl IssueDependencyCursorV1 {
    fn validate(&self) -> Result<(), String> {
        if self.display_number < 1 || self.issue_id.is_nil() {
            return Err("Issue dependency cursor is invalid".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListIssueDependenciesRequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    pub direction: IssueDependencyDirectionV1,
    #[serde(default)]
    pub page_direction: PageDirectionV1,
    #[serde(default)]
    pub cursor: Option<IssueDependencyCursorV1>,
    #[serde(default = "default_workspace_limit")]
    pub limit: u16,
}

impl ListIssueDependenciesRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_project_issue_ids(self.project_id, self.issue_id)?;
        validate_limit(self.limit)?;
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDependencyItemV1 {
    pub dependency: IssueDep,
    pub related_issue: Issue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDependencyPageV1 {
    pub items: Vec<IssueDependencyItemV1>,
    #[serde(default)]
    pub previous_cursor: Option<IssueDependencyCursorV1>,
    #[serde(default)]
    pub next_cursor: Option<IssueDependencyCursorV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueDependencyMutationRequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    pub depends_on_id: Uuid,
}

impl IssueDependencyMutationRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_project_issue_ids(self.project_id, self.issue_id)?;
        if self.depends_on_id.is_nil() {
            return Err("depends_on_id must not be nil".to_string());
        }
        if self.issue_id == self.depends_on_id {
            return Err("Issue cannot depend on itself".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDependencyMutationResultV1 {
    pub issue_id: Uuid,
    pub depends_on_id: Uuid,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListIssueEventsV2RequestV1 {
    pub project_id: Uuid,
    pub issue_id: Uuid,
    #[serde(default)]
    pub after_sequence: Option<i64>,
    #[serde(default = "default_workspace_limit")]
    pub limit: u16,
}

impl ListIssueEventsV2RequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_project_issue_ids(self.project_id, self.issue_id)?;
        validate_limit(self.limit)?;
        if self.after_sequence.is_some_and(|value| value < 0) {
            return Err("after_sequence must be nonnegative".to_string());
        }
        Ok(())
    }
}

pub type ListIssueEventsV2ResultV1 = IssueEventPageV1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkspaceErrorCodeV1 {
    InvalidRequest,
    NotFoundInProject,
    StaleVersion,
    IdempotencyConflict,
    NoSemanticChange,
    InvalidTransition,
    NotTerminal,
    NotArchived,
    DependencyCycle,
    DependencyEndpoint,
    AccessDenied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWorkspaceErrorV1 {
    pub code: IssueWorkspaceErrorCodeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_row_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_row_version: Option<i64>,
    pub retryable: bool,
    pub next_action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueTrackerStatusV1 {
    pub enabled: bool,
    pub tracker: String,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub next_poll_at: Option<DateTime<Utc>>,
    pub dispatched_count: usize,
    pub max_concurrent: usize,
    pub poll_interval_ms: u64,
    pub active_states: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDispatchRecordV1 {
    pub issue_id: String,
    pub issue_identifier: String,
    pub tracker: String,
    pub session_id: Uuid,
    pub dispatched_at: DateTime<Utc>,
    pub last_reconciled_at: Option<DateTime<Utc>>,
    pub terminal_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueTrackerTickResultV1 {
    pub issues_found: usize,
    pub dispatched: usize,
    pub skipped_claimed: usize,
    pub skipped_blocked: usize,
    pub errors: Vec<String>,
}

#[must_use]
pub fn operator_issue_create_id(project_id: Uuid, idempotency_key: &str) -> Uuid {
    Uuid::new_v5(
        &project_id,
        format!("{OPERATOR_ISSUE_CREATE_DOMAIN_V1}/{idempotency_key}").as_bytes(),
    )
}

#[must_use]
pub fn operator_issue_mutation_event_id(issue_id: Uuid, idempotency_key: &str) -> Uuid {
    Uuid::new_v5(
        &issue_id,
        format!("{OPERATOR_ISSUE_MUTATION_DOMAIN_V1}/{idempotency_key}").as_bytes(),
    )
}

const fn default_workspace_limit() -> u16 {
    ISSUE_WORKSPACE_DEFAULT_LIMIT
}

fn validate_limit(limit: u16) -> Result<(), String> {
    if !(1..=ISSUE_WORKSPACE_MAX_LIMIT).contains(&limit) {
        return Err("Issue workspace limit must be 1..=256".to_string());
    }
    Ok(())
}

fn validate_project_id(project_id: Uuid) -> Result<(), String> {
    if project_id.is_nil() {
        return Err("project_id must not be nil".to_string());
    }
    Ok(())
}

fn validate_project_issue_ids(project_id: Uuid, issue_id: Uuid) -> Result<(), String> {
    validate_project_id(project_id)?;
    if issue_id.is_nil() {
        return Err("issue_id must not be nil".to_string());
    }
    Ok(())
}

fn validate_idempotency_key(idempotency_key: &str) -> Result<(), String> {
    if idempotency_key.is_empty() || idempotency_key.len() > 128 || idempotency_key.contains('\0') {
        return Err("idempotency_key must be 1..=128 NUL-free bytes".to_string());
    }
    Ok(())
}

fn validate_mutation_identity(
    project_id: Uuid,
    issue_id: Uuid,
    expected_row_version: i64,
    idempotency_key: &str,
) -> Result<(), String> {
    validate_project_issue_ids(project_id, issue_id)?;
    if expected_row_version < 1 {
        return Err("expected_row_version must be positive".to_string());
    }
    validate_idempotency_key(idempotency_key)
}

fn validate_assignee(assignee: &str) -> Result<(), String> {
    if assignee.trim().is_empty() || assignee.len() > 256 || assignee.contains('\0') {
        return Err("Issue assignee must be nonblank, NUL-free, and at most 256 bytes".into());
    }
    Ok(())
}

fn validate_labels(labels: &[String]) -> Result<(), String> {
    if labels.len() > 64 {
        return Err("Issue labels must contain at most 64 entries".into());
    }
    if labels
        .iter()
        .any(|label| label.trim().is_empty() || label.len() > 128 || label.contains('\0'))
    {
        return Err("Issue labels must be nonblank, NUL-free, and at most 128 bytes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROJECT: &str = "11111111-1111-4111-8111-111111111111";
    const ISSUE: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn issue_workspace_nullable_patch_distinguishes_omitted_null_and_value() {
        let omitted: IssueContentPatchV2 = serde_json::from_value(json!({})).unwrap();
        assert_eq!(omitted.priority, NullablePatchV1::Unchanged);
        assert_eq!(omitted.assignee, NullablePatchV1::Unchanged);

        let clear: IssueContentPatchV2 =
            serde_json::from_value(json!({"priority": null, "assignee": null})).unwrap();
        assert_eq!(clear.priority, NullablePatchV1::Clear);
        assert_eq!(clear.assignee, NullablePatchV1::Clear);

        let set: IssueContentPatchV2 =
            serde_json::from_value(json!({"priority": 2, "assignee": "operator"})).unwrap();
        assert_eq!(set.priority, NullablePatchV1::Set(2));
        assert_eq!(set.assignee, NullablePatchV1::Set("operator".into()));
        assert_eq!(
            serde_json::to_value(clear).unwrap(),
            json!({"priority": null, "assignee": null})
        );
    }

    #[test]
    fn issue_workspace_non_nullable_patch_fields_reject_present_null() {
        for field in ["title", "body", "labels"] {
            let value = json!({field: null});
            assert!(serde_json::from_value::<IssueContentPatchV2>(value).is_err());
        }
        let patch: IssueContentPatchV2 = serde_json::from_value(json!({
            "title": "Visible issue title",
            "body": "Visible body",
            "labels": ["visible-label"]
        }))
        .unwrap();
        assert_eq!(patch.title.as_deref(), Some("Visible issue title"));
        assert_eq!(patch.body.as_deref(), Some("Visible body"));
        assert_eq!(patch.labels.unwrap(), vec!["visible-label"]);
    }

    #[test]
    fn issue_workspace_request_validation_enforces_bounds_and_cursor_kind() {
        let mut request: ListIssuesPageRequestV1 = serde_json::from_value(json!({
            "project_id": PROJECT
        }))
        .unwrap();
        assert_eq!(request.limit, 64);
        assert_eq!(request.archive, IssueArchiveFilterV1::Active);
        request.validate().unwrap();

        request.cursor = Some(IssueWorkspaceCursorV1::DisplayNumberAsc {
            display_number: 17,
            issue_id: Uuid::parse_str(ISSUE).unwrap(),
        });
        assert_eq!(
            request.validate().unwrap_err(),
            "Issue cursor kind must match sort"
        );
        request.sort = IssueWorkspaceSortV1::DisplayNumberAsc;
        request.validate().unwrap();
    }

    #[test]
    fn issue_workspace_new_requests_round_trip_with_exact_identity() {
        let request: UpdateIssueRequestV1 = serde_json::from_value(json!({
            "project_id": PROJECT,
            "issue_id": ISSUE,
            "expected_row_version": 4,
            "idempotency_key": "edit-visible-issue",
            "patch": {"title": "Visible issue title", "priority": null}
        }))
        .unwrap();
        request.validate().unwrap();
        let round_trip: UpdateIssueRequestV1 =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(round_trip, request);
        assert_eq!(round_trip.issue_id.to_string(), ISSUE);
        assert_eq!(
            round_trip.patch.title.as_deref(),
            Some("Visible issue title")
        );
        assert_eq!(round_trip.patch.priority, NullablePatchV1::Clear);
    }

    #[test]
    fn issue_workspace_tracker_shapes_use_current_daemon_fields() {
        let status: IssueTrackerStatusV1 = serde_json::from_value(json!({
            "enabled": true,
            "tracker": "local",
            "last_poll_at": "2026-09-10T12:00:00Z",
            "next_poll_at": "2026-09-10T12:00:30Z",
            "dispatched_count": 1,
            "max_concurrent": 5,
            "poll_interval_ms": 30000,
            "active_states": ["started"]
        }))
        .unwrap();
        assert_eq!(status.tracker, "local");
        assert_eq!(status.dispatched_count, 1);

        let dispatch: IssueDispatchRecordV1 = serde_json::from_value(json!({
            "issue_id": ISSUE,
            "issue_identifier": "LOCAL-17",
            "tracker": "local",
            "session_id": "33333333-3333-4333-8333-333333333333",
            "dispatched_at": "2026-09-10T12:00:01Z",
            "last_reconciled_at": null,
            "terminal_state": null
        }))
        .unwrap();
        assert_eq!(dispatch.issue_identifier, "LOCAL-17");

        let tick: IssueTrackerTickResultV1 = serde_json::from_value(json!({
            "issues_found": 3,
            "dispatched": 1,
            "skipped_claimed": 1,
            "skipped_blocked": 1,
            "errors": []
        }))
        .unwrap();
        assert_eq!((tick.issues_found, tick.dispatched), (3, 1));
    }

    #[test]
    fn issue_workspace_retry_ids_are_deterministic_and_domain_separated() {
        let project = Uuid::parse_str(PROJECT).unwrap();
        let issue = operator_issue_create_id(project, "create-visible-issue");
        assert_eq!(
            issue,
            operator_issue_create_id(project, "create-visible-issue")
        );
        let event = operator_issue_mutation_event_id(issue, "edit-visible-issue");
        assert_eq!(
            event,
            operator_issue_mutation_event_id(issue, "edit-visible-issue")
        );
        assert_ne!(issue, event);
    }
}
