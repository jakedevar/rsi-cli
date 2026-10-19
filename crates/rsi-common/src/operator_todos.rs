//! The operator to-do list (#1747): one durable, glanceable list of the things
//! only the operator can do (a login, a key to press, a decision), written by
//! managers and visible in the TUI wherever the operator is.
//!
//! Agents write it with `AgentOperatorTodoUpsert` and `AgentOperatorTodoClose`.
//! Both are open to any manager node seat (a portfolio seat of any tier, a
//! project's live PM, an area seat). The daemon derives the item's scope from
//! the caller's seat; no scope, project or author ever comes from request
//! JSON. An agent closes an item only with recorded evidence, and it marks an
//! item done only when it observed that the thing was done: it never does so
//! on the operator's behalf. The operator reads, adds, closes and (within a
//! short undo window, #2950) reopens items through `OperatorTodoList`,
//! `OperatorTodoAdd`, `OperatorTodoClose` and `OperatorTodoReopen`, which are
//! operator-only (outside `AGENT_VERBS`, `READ_VERBS`, native tools and the
//! agent CLI catalog).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The operator-only to-do RPCs (never in an agent catalog).
pub const OPERATOR_TODO_OPERATOR_METHODS: [&str; 4] = [
    "OperatorTodoList",
    "OperatorTodoAdd",
    "OperatorTodoClose",
    "OperatorTodoReopen",
];

pub const OPERATOR_TODO_MAX_TITLE_BYTES: usize = 512;
pub const OPERATOR_TODO_MAX_HOW_BYTES: usize = 2048;
pub const OPERATOR_TODO_MAX_LINK_BYTES: usize = 512;
pub const OPERATOR_TODO_MAX_EVIDENCE_BYTES: usize = 2048;
pub const OPERATOR_TODO_MAX_KEY_BYTES: usize = 128;
/// Most open items one scope may hold.
pub const OPERATOR_TODO_MAX_OPEN_PER_SCOPE: i64 = 64;
/// Most rows one `OperatorTodoList` returns.
pub const OPERATOR_TODO_LIST_LIMIT: usize = 256;

/// `scope` and `added_by` of an item the operator wrote.
pub const OPERATOR_TODO_OPERATOR: &str = "operator";
/// Prefix of `added_by` and `closed_by` for an agent session.
pub const OPERATOR_TODO_SESSION_PREFIX: &str = "session:";

/// The request is malformed (blank or oversized text, bad priority, a close
/// that names neither or both of `todo_id` and `todo_key`).
pub const OPERATOR_TODO_INVALID_REQUEST: &str = "operator_todo_invalid_request";
/// The caller holds no manager node seat.
pub const OPERATOR_TODO_NOT_AUTHORIZED: &str = "operator_todo_not_authorized";
/// No item matches (also returned for an item outside the caller's scope).
pub const OPERATOR_TODO_NOT_FOUND: &str = "operator_todo_not_found";
/// A project or area seat named an item that belongs to another scope.
pub const OPERATOR_TODO_NOT_IN_SCOPE: &str = "operator_todo_not_in_scope";
/// A reused idempotency key carries a different request.
pub const OPERATOR_TODO_IDEMPOTENCY_CONFLICT: &str = "operator_todo_idempotency_conflict";
/// An agent closed an item without evidence, or called it done without having
/// observed that it was done.
pub const OPERATOR_TODO_EVIDENCE_REQUIRED: &str = "operator_todo_evidence_required";
/// The item is already closed (done or dismissed). A closed row is immutable
/// (a store trigger); only the operator's undo (`OperatorTodoReopen`) brings an
/// item back, as a fresh open copy of the closed one.
pub const OPERATOR_TODO_CLOSED: &str = "operator_todo_closed";
/// `OperatorTodoReopen` named an item that is still open.
pub const OPERATOR_TODO_NOT_CLOSED: &str = "operator_todo_not_closed";
/// `OperatorTodoReopen` named an item an agent closed: only what the operator
/// closed can be undone, so a manager's close stays final.
pub const OPERATOR_TODO_REOPEN_NOT_OPERATOR_CLOSED: &str =
    "operator_todo_reopen_not_operator_closed";
/// `OperatorTodoReopen` came after the undo window.
pub const OPERATOR_TODO_REOPEN_WINDOW_ELAPSED: &str = "operator_todo_reopen_window_elapsed";
/// Seconds after the operator's close that the TUI offers an undo (#2950).
pub const OPERATOR_TODO_UNDO_WINDOW_SECS: i64 = 60;
/// Extra seconds the daemon allows past the window for a request that was
/// sent inside it (transport and scheduling delay).
pub const OPERATOR_TODO_REOPEN_GRACE_SECS: i64 = 15;
/// The scope already holds the most open items.
pub const OPERATOR_TODO_LIMIT: &str = "operator_todo_limit";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorTodoStatusV1 {
    Open,
    Done,
    Dismissed,
}

impl OperatorTodoStatusV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Done => "done",
            Self::Dismissed => "dismissed",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "open" => Some(Self::Open),
            "done" => Some(Self::Done),
            "dismissed" => Some(Self::Dismissed),
            _ => None,
        }
    }
}

/// One operator to-do item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorTodoV1 {
    pub id: Uuid,
    /// The node it concerns: `operator`, `portfolio:<id>`, `project:<id>` or
    /// `area:<id>`; derived by the daemon from the writer's seat.
    pub scope: String,
    /// The project it concerns when the scope is a project or the operator
    /// named one.
    pub project_id: Option<Uuid>,
    /// Stable per-scope slug a writer re-sends to update the same item.
    pub todo_key: String,
    pub title: String,
    /// One line: the exact command or keys.
    pub how: String,
    /// An Issue (`issue:#N`), a session (`session:<uuid>`) or a URL.
    pub link: Option<String>,
    /// 1 (most urgent) to 4; 3 when not given.
    pub priority: u8,
    /// `operator` or `session:<uuid>`.
    pub added_by: String,
    pub status: OperatorTodoStatusV1,
    /// What the closer observed (agents must give it; the operator may).
    pub evidence: Option<String>,
    /// The closer asserted that it observed the thing being done.
    pub evidence_observed: bool,
    pub closed_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.contains('\0')
}

fn valid_key(key: &str) -> bool {
    valid_text(key, OPERATOR_TODO_MAX_KEY_BYTES)
}

fn valid_priority(priority: Option<u8>) -> bool {
    priority.is_none_or(|p| (1..=4).contains(&p))
}

fn valid_link(link: Option<&String>) -> bool {
    link.is_none_or(|l| valid_text(l, OPERATOR_TODO_MAX_LINK_BYTES))
}

/// `AgentOperatorTodoUpsert {todo_key, title, how, link?, priority?,
/// idempotency_key}`: create the item `todo_key` in the caller's scope, or
/// update its title, how, link and priority while it is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperatorTodoUpsertRequestV1 {
    pub todo_key: String,
    pub title: String,
    pub how: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    pub idempotency_key: String,
}

impl AgentOperatorTodoUpsertRequestV1 {
    /// # Errors
    /// [`OPERATOR_TODO_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        if valid_key(&self.todo_key)
            && valid_text(&self.title, OPERATOR_TODO_MAX_TITLE_BYTES)
            && valid_text(&self.how, OPERATOR_TODO_MAX_HOW_BYTES)
            && valid_link(self.link.as_ref())
            && valid_priority(self.priority)
            && valid_key(&self.idempotency_key)
        {
            Ok(())
        } else {
            Err(OPERATOR_TODO_INVALID_REQUEST)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOperatorTodoUpsertResultV1 {
    pub todo: OperatorTodoV1,
    /// The call created the item (false: it updated an open one).
    pub created: bool,
    /// A replay under the same idempotency key.
    pub deduplicated: bool,
}

/// `AgentOperatorTodoClose {todo_id | todo_key, status, evidence, observed,
/// idempotency_key}`. `todo_key` names an item in the caller's own scope.
/// `evidence` is mandatory; `observed: true` is mandatory for `done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperatorTodoCloseRequestV1 {
    #[serde(default)]
    pub todo_id: Option<Uuid>,
    #[serde(default)]
    pub todo_key: Option<String>,
    /// `done` or `dismissed`.
    pub status: OperatorTodoStatusV1,
    pub evidence: String,
    #[serde(default)]
    pub observed: bool,
    pub idempotency_key: String,
}

impl AgentOperatorTodoCloseRequestV1 {
    /// # Errors
    /// [`OPERATOR_TODO_INVALID_REQUEST`], or [`OPERATOR_TODO_EVIDENCE_REQUIRED`]
    /// for a `done` that does not assert it was observed.
    pub fn validate(&self) -> Result<(), &'static str> {
        let one_target = match (&self.todo_id, &self.todo_key) {
            (Some(id), None) => !id.is_nil(),
            (None, Some(key)) => valid_key(key),
            _ => false,
        };
        if !one_target
            || self.status == OperatorTodoStatusV1::Open
            || !valid_key(&self.idempotency_key)
        {
            return Err(OPERATOR_TODO_INVALID_REQUEST);
        }
        if !valid_text(&self.evidence, OPERATOR_TODO_MAX_EVIDENCE_BYTES) {
            return Err(OPERATOR_TODO_EVIDENCE_REQUIRED);
        }
        if self.status == OperatorTodoStatusV1::Done && !self.observed {
            return Err(OPERATOR_TODO_EVIDENCE_REQUIRED);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOperatorTodoCloseResultV1 {
    pub todo: OperatorTodoV1,
    pub deduplicated: bool,
}

/// `OperatorTodoAdd` (operator-only): the operator writes an item too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorTodoAddRequestV1 {
    pub title: String,
    pub how: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    /// The project the item concerns, when any.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub idempotency_key: String,
}

impl OperatorTodoAddRequestV1 {
    /// # Errors
    /// [`OPERATOR_TODO_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        if valid_text(&self.title, OPERATOR_TODO_MAX_TITLE_BYTES)
            && valid_text(&self.how, OPERATOR_TODO_MAX_HOW_BYTES)
            && valid_link(self.link.as_ref())
            && valid_priority(self.priority)
            && self.project_id.is_none_or(|p| !p.is_nil())
            && valid_key(&self.idempotency_key)
        {
            Ok(())
        } else {
            Err(OPERATOR_TODO_INVALID_REQUEST)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorTodoAddResultV1 {
    pub todo: OperatorTodoV1,
    pub deduplicated: bool,
}

/// `OperatorTodoClose` (operator-only): mark any item done or dismissed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorTodoCloseRequestV1 {
    pub todo_id: Uuid,
    pub status: OperatorTodoStatusV1,
    #[serde(default)]
    pub note: Option<String>,
}

impl OperatorTodoCloseRequestV1 {
    /// # Errors
    /// [`OPERATOR_TODO_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.todo_id.is_nil()
            || self.status == OperatorTodoStatusV1::Open
            || self
                .note
                .as_ref()
                .is_some_and(|n| !valid_text(n, OPERATOR_TODO_MAX_EVIDENCE_BYTES))
        {
            Err(OPERATOR_TODO_INVALID_REQUEST)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorTodoCloseResultV1 {
    pub todo: OperatorTodoV1,
}

/// `OperatorTodoReopen` (operator-only, #2950): undo the operator's own close
/// of `todo_id` within [`OPERATOR_TODO_UNDO_WINDOW_SECS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorTodoReopenRequestV1 {
    pub todo_id: Uuid,
}

impl OperatorTodoReopenRequestV1 {
    /// # Errors
    /// [`OPERATOR_TODO_INVALID_REQUEST`].
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.todo_id.is_nil() {
            Err(OPERATOR_TODO_INVALID_REQUEST)
        } else {
            Ok(())
        }
    }
}

/// The reopened item is a new open row (new `id` and `todo_key`) carrying the
/// closed one's title, how, link, priority, project, author and creation time;
/// the closed row stays as it was. `deduplicated` is true when an earlier
/// reopen of the same item answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorTodoReopenResultV1 {
    pub todo: OperatorTodoV1,
    pub deduplicated: bool,
}

/// `OperatorTodoList` (operator-only): every open item, plus the items closed
/// at or after `closed_since` (the catch-up view).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorTodoListRequestV1 {
    #[serde(default)]
    pub closed_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorTodoListResultV1 {
    /// Open items, most urgent then newest first.
    pub open: Vec<OperatorTodoV1>,
    /// Items closed at or after `closed_since`, newest close first.
    pub recently_closed: Vec<OperatorTodoV1>,
    /// More rows exist than one list returns.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert() -> AgentOperatorTodoUpsertRequestV1 {
        AgentOperatorTodoUpsertRequestV1 {
            todo_key: "codex-login".into(),
            title: "Log in to Codex".into(),
            how: "codex login".into(),
            link: None,
            priority: Some(2),
            idempotency_key: "k1".into(),
        }
    }

    #[test]
    fn upsert_validation_rejects_blank_oversized_and_bad_priority() {
        assert_eq!(upsert().validate(), Ok(()));
        for broken in [
            AgentOperatorTodoUpsertRequestV1 {
                title: "  ".into(),
                ..upsert()
            },
            AgentOperatorTodoUpsertRequestV1 {
                how: "x".repeat(OPERATOR_TODO_MAX_HOW_BYTES + 1),
                ..upsert()
            },
            AgentOperatorTodoUpsertRequestV1 {
                priority: Some(0),
                ..upsert()
            },
            AgentOperatorTodoUpsertRequestV1 {
                priority: Some(5),
                ..upsert()
            },
            AgentOperatorTodoUpsertRequestV1 {
                todo_key: "a\0b".into(),
                ..upsert()
            },
        ] {
            assert_eq!(broken.validate(), Err(OPERATOR_TODO_INVALID_REQUEST));
        }
    }

    #[test]
    fn agent_close_needs_one_target_evidence_and_an_observed_done() {
        let close = AgentOperatorTodoCloseRequestV1 {
            todo_id: None,
            todo_key: Some("codex-login".into()),
            status: OperatorTodoStatusV1::Done,
            evidence: "[observed] `codex login status` prints a session".into(),
            observed: true,
            idempotency_key: "k2".into(),
        };
        assert_eq!(close.validate(), Ok(()));
        let both = AgentOperatorTodoCloseRequestV1 {
            todo_id: Some(Uuid::new_v4()),
            ..close.clone()
        };
        assert_eq!(both.validate(), Err(OPERATOR_TODO_INVALID_REQUEST));
        let neither = AgentOperatorTodoCloseRequestV1 {
            todo_key: None,
            ..close.clone()
        };
        assert_eq!(neither.validate(), Err(OPERATOR_TODO_INVALID_REQUEST));
        let reopen = AgentOperatorTodoCloseRequestV1 {
            status: OperatorTodoStatusV1::Open,
            ..close.clone()
        };
        assert_eq!(reopen.validate(), Err(OPERATOR_TODO_INVALID_REQUEST));
        let no_evidence = AgentOperatorTodoCloseRequestV1 {
            evidence: " ".into(),
            ..close.clone()
        };
        assert_eq!(no_evidence.validate(), Err(OPERATOR_TODO_EVIDENCE_REQUIRED));
        let unobserved = AgentOperatorTodoCloseRequestV1 {
            observed: false,
            ..close.clone()
        };
        assert_eq!(unobserved.validate(), Err(OPERATOR_TODO_EVIDENCE_REQUIRED));
        // A dismissal records a reason but asserts nothing was done.
        let dismissed = AgentOperatorTodoCloseRequestV1 {
            status: OperatorTodoStatusV1::Dismissed,
            observed: false,
            evidence: "superseded by a newer item".into(),
            ..close
        };
        assert_eq!(dismissed.validate(), Ok(()));
    }

    #[test]
    fn requests_reject_unknown_fields_so_scope_cannot_be_forged() {
        for forged in ["scope", "project_id", "added_by"] {
            let mut value = serde_json::json!({
                "todo_key": "k", "title": "t", "how": "h", "idempotency_key": "i"
            });
            value[forged] = serde_json::json!("project:forged");
            assert!(
                serde_json::from_value::<AgentOperatorTodoUpsertRequestV1>(value).is_err(),
                "{forged} must not be accepted"
            );
        }
    }
}
