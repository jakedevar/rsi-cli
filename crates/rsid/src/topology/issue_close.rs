//! Closing the input Issue when a land publishes (#2260, plan §3.1).
//!
//! The policy is "close an Issue when its fix is on rolling". A land step with
//! `close_issue` closes the Issue that `inputs.issue` named at accept, as the
//! execution's owner (the manager or Epic lead that requested it; for an
//! operator start the project manager resolved at accept, #1746), through the
//! same guarded `AgentUpdateIssueStatus` path an owner would call by hand. The
//! Issue layer re-derives the owner's live Issue authority in the write
//! transaction, so a paused, replaced or ungranted owner is refused there and
//! the refusal is data: the land still settles `succeeded`, its output says
//! `issue_close: {status: "refused", code}` and the Issue stays open for the
//! owner (never a failed land: the commit is on rolling either way).
//!
//! Outcomes are idempotent, so a crash between the close and the land's
//! settlement re-runs the close on the next tick and the land settles once:
//!
//! - `closed`: this execution's close wrote the Issue (or already had: the
//!   write is keyed by the execution and attempt, and an Issue that is Closed
//!   with that key on its event log is this close replayed);
//! - `already_closed`: the Issue was Closed by someone else; nothing written;
//! - `skipped`: the Issue is Cancelled, archived or gone; left untouched;
//! - `refused`: the owner may not write the Issue (or there is no owner).
//!
//! A transient store error is an `Err` the caller retries on its next tick.

use rsi_common::types::IssueStatus;
use rusqlite::params;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::topology::steps::WorkflowSteps;
use crate::topology::store::{AttemptRow, ExecutionRow};

/// What a land's `close_issue` does to the input Issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IssueClose {
    /// Closed by this execution's land.
    Closed,
    /// Already Closed by someone else; nothing written.
    AlreadyClosed,
    /// Left untouched (cancelled, archived or missing); carries the reason.
    Skipped(&'static str),
    /// The owner may not close it; carries the Issue layer's code.
    Refused(String),
}

impl IssueClose {
    pub(crate) const fn status(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::AlreadyClosed => "already_closed",
            Self::Skipped(_) => "skipped",
            Self::Refused(_) => "refused",
        }
    }

    pub(crate) fn code(&self) -> Option<&str> {
        match self {
            Self::Skipped(code) => Some(code),
            Self::Refused(code) => Some(code),
            Self::Closed | Self::AlreadyClosed => None,
        }
    }

    /// The land output's `issue_close` field.
    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({ "status": self.status() });
        if let Some(code) = self.code() {
            value["code"] = code.into();
        }
        value
    }
}

/// The Issue a land closes and who closes it.
#[derive(Clone, Debug)]
pub(crate) struct IssueCloseRequest {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    /// The execution's owner (`requested_by_session_id`), if it has one.
    pub(crate) owner: Option<Uuid>,
    pub(crate) project_id: Uuid,
    pub(crate) display_number: i64,
    /// The Issue status note (#2651: the evidence commit and the worker's
    /// verification lines for an already-satisfied close); `None` for a land.
    pub(crate) note: Option<String>,
    /// Prefix of the idempotency key: a land and an already-satisfied close of
    /// the same attempt never share a key.
    pub(crate) key_kind: &'static str,
}

/// The longest verification excerpt an already-satisfied note carries; the
/// whole note stays within `ISSUE_STATUS_NOTE_MAX_BYTES_V1`.
const SATISFIED_NOTE_EXCERPT_BYTES: usize = 1_600;

/// #2651: the note an already-satisfied close writes on the Issue: the
/// evidence commit and the worker's own handoff lines (its verification
/// claims), without the marker and path fields.
pub(crate) fn already_satisfied_note(evidence_commit: &str, worker_message: &str) -> String {
    let handoff = worker_message
        .find("PIPELINE HANDOFF")
        .map_or("", |at| &worker_message[at..]);
    let mut excerpt = String::new();
    for line in handoff.lines().skip(1) {
        let key = line.split(':').next().unwrap_or("").trim();
        if matches!(
            key,
            "doc_path" | "manifest_path" | "status" | "evidence_commit"
        ) {
            continue;
        }
        if excerpt.len() + line.len() + 1 > SATISFIED_NOTE_EXCERPT_BYTES {
            break;
        }
        excerpt.push_str(line.trim_end());
        excerpt.push('\n');
    }
    let mut note = format!(
        "Already satisfied on rolling (topology run, no landing): evidence commit {evidence_commit}."
    );
    if !excerpt.trim().is_empty() {
        note.push_str("\nWorker verification:\n");
        note.push_str(excerpt.trim_end());
    }
    note.replace('\0', "")
}

impl IssueCloseRequest {
    /// The close a worker's `already_satisfied` handoff asks for (#2651): the
    /// execution's input Issue, written under a key of its own attempt.
    /// `Err(refused)` when the execution carries no Issue snapshot.
    pub(crate) fn for_already_satisfied(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        note: String,
    ) -> std::result::Result<Self, IssueClose> {
        let (project_id, display_number) =
            crate::topology::starters::issue_snapshot(execution.input.as_ref())
                .ok_or_else(|| IssueClose::Refused("issue_input_missing".into()))?;
        Ok(Self {
            execution_id: execution.id,
            attempt_id: attempt.id,
            owner: execution.requested_by_session_id,
            project_id,
            display_number,
            note: Some(note),
            key_kind: "topology-satisfied",
        })
    }

    /// The close `attempt`'s land step asks for: `None` when it does not close
    /// an Issue; `Some(Err(refused))` when it does but the execution carries
    /// no Issue snapshot (accept refuses that, so only a hand-built execution
    /// reaches it).
    pub(crate) fn for_land(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Option<std::result::Result<Self, IssueClose>> {
        let closes = WorkflowSteps::from_workflow(&execution.definition)
            .is_ok_and(|steps| steps.close_issue(&attempt.node_id));
        closes.then(|| {
            let (project_id, display_number) =
                crate::topology::starters::issue_snapshot(execution.input.as_ref())
                    .ok_or_else(|| IssueClose::Refused("issue_input_missing".into()))?;
            Ok(Self {
                execution_id: execution.id,
                attempt_id: attempt.id,
                owner: execution.requested_by_session_id,
                project_id,
                display_number,
                note: None,
                key_kind: "topology-land",
            })
        })
    }

    /// The Issue write's idempotency key: it names the execution and the land
    /// attempt, and the owner's Issue event log keeps it.
    pub(crate) fn idempotency_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.key_kind, self.execution_id, self.attempt_id
        )
    }
}

/// An Issue-layer error that is a refusal (authority, scope, transition), not a
/// transient condition: `Some(code)`. A stale version or storage failure is
/// retried on the next tick, as is any other error.
fn refusal_code(error: &DaemonError) -> Option<String> {
    let code = match error {
        DaemonError::StructuredRpc { message, .. } => message.as_str(),
        DaemonError::PolicyDenied(message) | DaemonError::InvalidParam(message) => message.as_str(),
        _ => return None,
    };
    let issue_layer = code.starts_with("agent_issue_")
        || code == rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE;
    let transient = matches!(
        code,
        "agent_issue_stale_version" | "agent_issue_storage_failure"
    );
    (issue_layer && !transient).then(|| code.to_owned())
}

/// Close the Issue as the owner. Pure over rows; call it under the store lock
/// so the read, the guarded write and the replay check see one state.
///
/// # Errors
/// A transient store error (including a stale row version): ask again.
pub(crate) fn close_input_issue(store: &Store, request: &IssueCloseRequest) -> Result<IssueClose> {
    let Some(owner) = request.owner else {
        return Ok(IssueClose::Refused("owner_required".into()));
    };
    // #2472: a rotated portfolio seat closes through its node's current seat.
    let owner = crate::topology::agent::live_owner(store, owner)?;
    let Some(issue) =
        store.get_issue_by_display_number(request.project_id, request.display_number)?
    else {
        return Ok(IssueClose::Skipped("issue_missing"));
    };
    if issue.archived_at.is_some() {
        return Ok(IssueClose::Skipped("issue_archived"));
    }
    let key = request.idempotency_key();
    match issue.status {
        IssueStatus::Cancelled => return Ok(IssueClose::Skipped("issue_cancelled")),
        IssueStatus::Closed => {
            // Closed with this close's own key on its event log: the write
            // committed before a crash, so this is the close replayed.
            let ours: bool = store.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM issue_events \
                 WHERE issue_id=?1 AND actor_session_id=?2 AND idempotency_key=?3)",
                params![issue.id.to_string(), owner.to_string(), key],
                |row| row.get(0),
            )?;
            return Ok(if ours {
                IssueClose::Closed
            } else {
                IssueClose::AlreadyClosed
            });
        }
        IssueStatus::Open | IssueStatus::InProgress => {}
    }
    let written = store.agent_update_issue_status(
        owner,
        &rsi_common::rpc::AgentUpdateIssueStatusRequestV1 {
            note: request.note.clone(),
            issue_id: Some(issue.id),
            display_number: None,
            status: IssueStatus::Closed,
            expected_row_version: issue.row_version,
            idempotency_key: key,
            include_issue: false,
            project_id: Some(request.project_id),
        },
    );
    match written {
        Ok(_) => Ok(IssueClose::Closed),
        Err(error) => refusal_code(&error).map_or(Err(error), |code| Ok(IssueClose::Refused(code))),
    }
}
