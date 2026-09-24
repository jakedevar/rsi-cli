//! K14 (#672): operator RPC delegation for the current appointed manager.
//!
//! The wire form (`OperatorCallV1`) is an open `{method, params}` pair so a
//! refused method reaches the daemon and is answered with a typed code. The
//! only executable form is the closed, versioned `DelegatedOperatorCallV1`
//! allowlist; a method without a variant has no daemon code path at all.
#![allow(clippy::missing_errors_doc)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::archive_cleanup::{ArchiveSessionParamsV1, GetArchiveCleanupStatusParamsV1};
use crate::types::{SandboxCleanupState, SessionKind, SessionStatus};

/// Version of the current delegable operator-method allowlist.
pub const DELEGATED_OPERATOR_ALLOWLIST_VERSION: u32 = 2;

/// K14a allowlist (v1), kept as the historical record of what v1 delegated.
pub const DELEGABLE_OPERATOR_METHODS_V1: [&str; 3] =
    ["ArchiveSession", "GetArchiveCleanupStatus", "ListSessions"];

/// K14b allowlist (v2): v1 plus the logical `UnarchiveSession`.
pub const DELEGABLE_OPERATOR_METHODS_V2: [&str; 4] = [
    "ArchiveSession",
    "GetArchiveCleanupStatus",
    "ListSessions",
    "UnarchiveSession",
];

/// The current allowlist: exactly the method names of
/// `DelegatedOperatorCallV1`, sorted.
pub const DELEGABLE_OPERATOR_METHODS: &[&str] = &DELEGABLE_OPERATOR_METHODS_V2;

/// Operator RPC families that stay undelegable even with the grant.
///
/// Self-escalation, human gates, scope moves, daemon config, launches or spend
/// outside quotas, continuation outside manager gates, deletion, daemon-global
/// custody, operator kernels and streaming. Disjoint from the allowlist.
pub const NEVER_DELEGABLE_OPERATOR_METHODS_V1: &[&str] = &[
    // Appointment, scope, policy, grant and decisions.
    "ConfigureHarnessManager",
    "ConfigureHarnessManagerPolicy",
    "GetHarnessManager",
    "GetHarnessManagerPolicy",
    "ListHarnessManagerEpics",
    "ListHarnessManagerScope",
    "GetHarnessManagerState",
    "AnswerHarnessManagerDecision",
    "AnswerQuestion",
    // Scope self-escalation.
    "SetSessionParent",
    "UpdateSessionProject",
    "CreateContainer",
    "SetEpicLead",
    // Daemon configuration and spend policy.
    "GetDaemonConfig",
    "UpdateDaemonConfig",
    "UpdateModelControlPolicy",
    // Launch or spend outside manager quotas.
    "LaunchSession",
    "ExecuteTopology",
    "ExecuteWorkflow",
    "StartChainedWorkflow",
    "RunRecursiveFakeScheduler",
    "RunRecursiveLiveScheduler",
    "RunRecursiveTopologyNodeFakeScheduler",
    "CreateScheduledJob",
    "UpdateScheduledJob",
    "DeleteScheduledJob",
    "ToggleScheduledJob",
    "TriggerScheduledJob",
    "GenerateText",
    "CompilePrompt",
    "GenerateWorkflow",
    "RefineWorkflow",
    "TriggerDream",
    // Continuation/interrupt outside manager gates (K14_PAUSE).
    "ContinueSession",
    "InterruptSession",
    "MarkPendingArchive",
    // Deletion (never delete rows).
    "PurgeSession",
    "DeleteSession",
    "UndeleteSession",
    "DeleteProject",
    "DeleteLabel",
    "DeleteTopology",
    // Daemon-global custody (K14_SCOPE).
    "GetSandboxStorageStatus",
    "RunSandboxBuildCacheReclaim",
    "ListSourceWorktreeCohorts",
    "AuditSourceWorktreeCohort",
    "ApplySourceWorktreeCohort",
    "GetSourceWorktreeSettlementRun",
    // ProgramRun and Closure kernels.
    "CreateProgramRun",
    "GetProgramRun",
    "ListProgramRuns",
    "ListProgramRunTransitions",
    "GetProgramRunOperationalStatus",
    "CancelProgramRun",
    "ResumeBlockedProgramRun",
    "ReconcileProgramRuns",
    "CreateClosureProgram",
    "UpdateClosureProgram",
    "LaunchClosureSource",
    "ListClosurePrograms",
    "GetClosureProgram",
    "RecordClosureEvidence",
    // Generic Issue verbs (guarded IssueCoordinate path exists).
    "CreateIssue",
    "GetIssueInProject",
    "ListIssuesPage",
    "UpdateIssue",
    "ListIssueDependencies",
    "AddIssueDependency",
    "RemoveIssueDependency",
    "ArchiveIssue",
    "RestoreIssue",
    "GetIssue",
    "ListIssues",
    "UpdateIssueStatus",
    "AddIssueDep",
    "RemoveIssueDep",
    "ListReadyIssues",
    "LinkIssueToIdea",
    "ListIssueEvents",
    // Streaming, memory writes and caches.
    "Subscribe",
    "SetEntityCard",
    "MemoryIndex",
    "ClearGraphCache",
];

/// Refusal for any method outside `DELEGABLE_OPERATOR_METHODS`.
pub const OPERATOR_METHOD_NOT_DELEGABLE: &str = "manager_v2_operator_method_not_delegable";
/// Refusal for malformed params of an allowlisted method.
pub const OPERATOR_PARAMS_INVALID: &str = "manager_v2_operator_params_invalid";

/// Byte bound of a `ListSessions` page envelope.
pub const DELEGATED_PAGE_MAX_BYTES: usize = 12 * 1024;
/// Row bound of a `ListSessions` page.
pub const DELEGATED_PAGE_MAX_ROWS: u16 = 64;
/// Byte bound of a serialized `OperatorCallResultV1` in a receipt.
pub const OPERATOR_RESULT_MAX_BYTES: usize = 16 * 1024;
const OPERATOR_PARAMS_MAX_BYTES: usize = 4096;

/// Wire form carried by `ManagerActionV2::OperatorCall`.
///
/// An allowlisted method's params decode strictly (closed DTOs, so nested authority fields
/// are rejected at decode); any other method still decodes so the daemon can
/// answer it with `OPERATOR_METHOD_NOT_DELEGABLE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorCallV1 {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl<'de> Deserialize<'de> for OperatorCallV1 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            method: String,
            #[serde(default)]
            params: Value,
        }
        let wire = Wire::deserialize(deserializer)?;
        let call = Self {
            method: wire.method,
            params: wire.params,
        };
        if DELEGABLE_OPERATOR_METHODS.contains(&call.method.as_str()) {
            call.typed().map_err(serde::de::Error::custom)?;
        }
        Ok(call)
    }
}

impl OperatorCallV1 {
    /// Shape bounds only; the method allowlist is enforced by `typed()`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.method.is_empty() || self.method.len() > 64 {
            return Err(OPERATOR_PARAMS_INVALID);
        }
        if serde_json::to_vec(&self.params).map_or(true, |v| v.len() > OPERATOR_PARAMS_MAX_BYTES) {
            return Err(OPERATOR_PARAMS_INVALID);
        }
        Ok(())
    }

    /// Resolve the closed executable form, refusing every other method.
    pub fn typed(&self) -> Result<DelegatedOperatorCallV1, &'static str> {
        self.validate()?;
        let params = if self.params.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            self.params.clone()
        };
        let call = match self.method.as_str() {
            "ArchiveSession" => DelegatedOperatorCallV1::ArchiveSession(decode(params)?),
            "GetArchiveCleanupStatus" => {
                DelegatedOperatorCallV1::GetArchiveCleanupStatus(decode(params)?)
            }
            "ListSessions" => DelegatedOperatorCallV1::ListSessions(decode(params)?),
            "UnarchiveSession" => DelegatedOperatorCallV1::UnarchiveSession(decode(params)?),
            _ => return Err(OPERATOR_METHOD_NOT_DELEGABLE),
        };
        call.validate()?;
        Ok(call)
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, &'static str> {
    serde_json::from_value(value).map_err(|_| OPERATOR_PARAMS_INVALID)
}

/// Params of the delegated `UnarchiveSession` (same shape as the operator
/// RPC's daemon-local params; closed so nested authority fails at decode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnarchiveSessionParamsV1 {
    pub session_id: Uuid,
}

/// Session-targeted fence for mutating delegated calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCallFenceV1 {
    pub session_updated_at: DateTime<Utc>,
}

/// Closed executable allowlist (v1). Params reuse the operator handler's own
/// params type where one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedOperatorCallV1 {
    /// Logical-only archive of one project leaf (never worktree cleanup).
    ArchiveSession(ArchiveSessionParamsV1),
    /// K14b: logical restore of one Archived project leaf to Completed
    /// (the housekeeping `RestoreSession` effect; never a worktree rebuild).
    UnarchiveSession(UnarchiveSessionParamsV1),
    GetArchiveCleanupStatus(GetArchiveCleanupStatusParamsV1),
    /// Project-bound, byte-bounded session page.
    ListSessions(DelegatedListSessionsParamsV1),
}

impl DelegatedOperatorCallV1 {
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::ArchiveSession(_) => "ArchiveSession",
            Self::GetArchiveCleanupStatus(_) => "GetArchiveCleanupStatus",
            Self::ListSessions(_) => "ListSessions",
            Self::UnarchiveSession(_) => "UnarchiveSession",
        }
    }

    /// The project session a call targets, if any.
    #[must_use]
    pub const fn session_id(&self) -> Option<Uuid> {
        match self {
            Self::ArchiveSession(p) => Some(p.session_id),
            Self::UnarchiveSession(p) => Some(p.session_id),
            Self::GetArchiveCleanupStatus(p) => Some(p.session_id),
            Self::ListSessions(_) => None,
        }
    }

    /// True for calls that change state and therefore require a fence.
    #[must_use]
    pub const fn is_effect(&self) -> bool {
        matches!(self, Self::ArchiveSession(_) | Self::UnarchiveSession(_))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::ArchiveSession(p) if p.session_id.is_nil() => Err(OPERATOR_PARAMS_INVALID),
            Self::UnarchiveSession(p) if p.session_id.is_nil() => Err(OPERATOR_PARAMS_INVALID),
            Self::GetArchiveCleanupStatus(p) if p.session_id.is_nil() => {
                Err(OPERATOR_PARAMS_INVALID)
            }
            Self::ListSessions(p) => p.validate(),
            _ => Ok(()),
        }
    }
}

/// Keyset cursor `(updated_at, id)` over the raw stored timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedSessionCursorV1 {
    pub updated_at: String,
    pub id: Uuid,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedListSessionsParamsV1 {
    /// Empty means every status except `Deleted`.
    #[serde(default)]
    pub status_in: Vec<SessionStatus>,
    /// Only rows whose `updated_at` is strictly older than this instant.
    #[serde(default)]
    pub terminal_before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub after: Option<DelegatedSessionCursorV1>,
    /// At most `DELEGATED_PAGE_MAX_ROWS`; defaults to it.
    #[serde(default)]
    pub limit: Option<u16>,
}

impl DelegatedListSessionsParamsV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.status_in.len() > 8
            || self
                .limit
                .is_some_and(|l| l == 0 || l > DELEGATED_PAGE_MAX_ROWS)
            || self
                .after
                .as_ref()
                .is_some_and(|c| c.updated_at.is_empty() || c.updated_at.len() > 40)
        {
            return Err(OPERATOR_PARAMS_INVALID);
        }
        Ok(())
    }
}

/// Fixed-width projection; no free text beyond a bounded refusal code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedSessionRowV1 {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub kind: SessionKind,
    pub status: SessionStatus,
    pub updated_at: String,
    pub last_activity_at: String,
    pub pinned: bool,
    pub sandbox_cleanup_state: Option<SandboxCleanupState>,
    /// Why a delegated logical `ArchiveSession` would be refused now.
    pub archive_blocker: Option<String>,
}

/// Bounded result carried by a succeeded `operator_call` receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorCallResultV1 {
    Scalar {
        method: String,
        result: Value,
    },
    Page {
        method: String,
        rows: Vec<DelegatedSessionRowV1>,
        next_after: Option<DelegatedSessionCursorV1>,
        row_count: u16,
    },
}

impl OperatorCallResultV1 {
    /// A result that cannot fit is refused, never truncated.
    pub fn validate(&self) -> Result<(), &'static str> {
        let size = serde_json::to_vec(self).map_err(|_| "manager_v2_operator_result_invalid")?;
        if size.len() > OPERATOR_RESULT_MAX_BYTES {
            return Err("manager_v2_operator_result_too_large");
        }
        if let Self::Page {
            rows, row_count, ..
        } = self
            && (rows.len() > usize::from(DELEGATED_PAGE_MAX_ROWS)
                || usize::from(*row_count) != rows.len())
        {
            return Err("manager_v2_operator_result_invalid");
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(method: &str, params: Value) -> OperatorCallV1 {
        OperatorCallV1 {
            method: method.into(),
            params,
        }
    }

    #[test]
    fn allowlist_names_match_the_closed_enum() {
        let id = Uuid::new_v4();
        let mut methods = vec![
            call("ArchiveSession", json!({"session_id": id})),
            call("GetArchiveCleanupStatus", json!({"session_id": id})),
            call("ListSessions", json!({})),
            call("UnarchiveSession", json!({"session_id": id})),
        ]
        .into_iter()
        .map(|c| c.typed().unwrap().method())
        .collect::<Vec<_>>();
        methods.sort_unstable();
        assert_eq!(methods, DELEGABLE_OPERATOR_METHODS);
        assert_eq!(DELEGABLE_OPERATOR_METHODS, DELEGABLE_OPERATOR_METHODS_V2);
        // v2 extends v1 by exactly UnarchiveSession.
        assert!(
            DELEGABLE_OPERATOR_METHODS_V1
                .iter()
                .all(|m| DELEGABLE_OPERATOR_METHODS_V2.contains(m))
        );
        assert_eq!(DELEGATED_OPERATOR_ALLOWLIST_VERSION, 2);
    }

    #[test]
    fn never_delegable_is_disjoint_and_every_name_refuses() {
        for method in NEVER_DELEGABLE_OPERATOR_METHODS_V1 {
            assert!(!DELEGABLE_OPERATOR_METHODS.contains(method), "{method}");
            assert_eq!(
                call(method, json!({})).typed(),
                Err(OPERATOR_METHOD_NOT_DELEGABLE),
                "{method}"
            );
        }
    }

    #[test]
    fn allowlisted_params_are_closed() {
        let id = Uuid::new_v4();
        assert_eq!(
            call("ArchiveSession", json!({"session_id": id, "force": true})).typed(),
            Err(OPERATOR_PARAMS_INVALID)
        );
        assert_eq!(
            call("ListSessions", json!({"limit": 65})).typed(),
            Err(OPERATOR_PARAMS_INVALID)
        );
        let typed = call(
            "ListSessions",
            json!({"status_in": ["Completed"], "limit": 10}),
        )
        .typed()
        .unwrap();
        assert_eq!(typed.method(), "ListSessions");
        assert!(!typed.is_effect());
        let archive = call("ArchiveSession", json!({"session_id": id}))
            .typed()
            .unwrap();
        assert_eq!(archive.session_id(), Some(id));
        assert!(archive.is_effect());
    }

    #[test]
    fn allowlisted_params_reject_nested_authority_at_decode() {
        let id = Uuid::new_v4();
        let ok: OperatorCallV1 =
            serde_json::from_value(json!({"method":"ArchiveSession","params":{"session_id":id}}))
                .unwrap();
        assert_eq!(ok.typed().unwrap().session_id(), Some(id));
        for forged in [
            json!({"method":"ArchiveSession","params":{"session_id":id,"project_id":id}}),
            json!({"method":"ListSessions","params":{"after":{"updated_at":"t","id":id,"caller_session_id":id}}}),
            json!({"method":"ListSessions","params":{},"permissions":["all"]}),
        ] {
            assert!(
                serde_json::from_value::<OperatorCallV1>(forged.clone()).is_err(),
                "{forged}"
            );
        }
        // A refused method still decodes, so the daemon answers it typed.
        let refused: OperatorCallV1 =
            serde_json::from_value(json!({"method":"UpdateDaemonConfig","params":{"field":"x"}}))
                .unwrap();
        assert_eq!(refused.typed(), Err(OPERATOR_METHOD_NOT_DELEGABLE));
    }

    #[test]
    fn result_bound_refuses_instead_of_truncating() {
        let small = OperatorCallResultV1::Scalar {
            method: "GetArchiveCleanupStatus".into(),
            result: json!({"ok": true}),
        };
        assert_eq!(small.validate(), Ok(()));
        let large = OperatorCallResultV1::Scalar {
            method: "GetArchiveCleanupStatus".into(),
            result: json!("x".repeat(OPERATOR_RESULT_MAX_BYTES)),
        };
        assert_eq!(
            large.validate(),
            Err("manager_v2_operator_result_too_large")
        );
    }
}
