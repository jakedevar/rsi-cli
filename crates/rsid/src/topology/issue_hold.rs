//! One Issue, one live run (#2427).
//!
//! An execution whose `inputs.issue` names an Issue holds it from accept until
//! it settles. [`guard`] refuses a second execute while another execution or an
//! Issue-bound worker (#2212) holds the Issue, unless the request names the
//! holding execution in `supersedes_execution`, which is interrupted first.
//! [`mark_in_progress`] moves the Issue to `InProgress` at accept so a manager
//! scanning Open Issues does not route it again; [`release`] returns it to
//! `Open` when the execution ends `failed` or `cancelled` with nothing else
//! holding it. Both writes are best effort: the Issue note is bookkeeping, not
//! a reason to fail an execution.

use rsi_common::rpc::AgentUpdateIssueStatusRequestV1;
use rsi_common::types::IssueStatus;
use uuid::Uuid;

use crate::error::Result;
use crate::store::Store;
use crate::topology::agent::{Reach, TopologyCaller, authorize_execution, refusal};
use crate::topology::store::{self as rows, ExecutionStatus};
use rsi_common::types::GraphExecutionUpdate;

/// The interrupt updates of one superseded execution, for the caller to publish.
pub(crate) type Superseded = (Uuid, Vec<GraphExecutionUpdate>);

pub(crate) const ALREADY_LIVE: &str = "topology_issue_already_live";

fn already_live(holder: &str) -> crate::error::DaemonError {
    refusal(
        ALREADY_LIVE,
        &format!(
            "{holder}; wait for it to settle, or pass supersedes_execution with the execution id to interrupt and replace it"
        ),
    )
}

/// Admit or refuse `inputs.issue` under the accepting lock; interrupts the
/// execution `supersedes` names when it is the Issue's holder.
///
/// # Errors
/// `topology_issue_already_live` naming the holder, or `topology_supersedes_not_live`
/// when `supersedes` names an execution that does not hold the Issue.
pub(crate) fn guard(
    store: &Store,
    caller_id: Uuid,
    caller: &TopologyCaller,
    new_id: Uuid,
    supersedes: Option<Uuid>,
    inputs: &serde_json::Value,
) -> Result<Vec<Superseded>> {
    let snapshot = crate::topology::starters::issue_snapshot(Some(inputs));
    let Some((project, number)) = snapshot else {
        return match supersedes {
            None => Ok(Vec::new()),
            Some(_) => Err(refusal(
                "topology_supersedes_not_live",
                "supersedes_execution needs inputs.issue naming the Issue the old execution holds",
            )),
        };
    };
    let live = store.live_topology_executions_for_issue(project, number)?;
    if let Some(named) = supersedes
        && !live.iter().any(|(id, _, _)| *id == named)
    {
        return Err(refusal(
            "topology_supersedes_not_live",
            "supersedes_execution must name a live execution that holds inputs.issue; omit it or list the Issue's executions",
        ));
    }
    if let Some((id, status, _)) = live.iter().find(|(id, _, _)| Some(*id) != supersedes) {
        return Err(already_live(&format!(
            "topology execution {id} is {status} and holds Issue #{number}"
        )));
    }
    if let Some(issue) = store.get_issue_by_display_number(project, number)?
        && let Some(holder) = store.live_issue_worker_holder(issue.id)?
    {
        return Err(already_live(&holder));
    }
    let mut interrupted = Vec::new();
    for (id, _, row_version) in live {
        authorize_execution(store, caller_id, id, Reach::Stop)?;
        let (recorded, _) = rows::request_agent_interrupt(
            store,
            id,
            row_version,
            &format!("supersede:{new_id}"),
            caller.actor(),
        )?;
        let updates = match recorded {
            rows::Recorded::Fresh(updates) => updates,
            rows::Recorded::Replay => Vec::new(),
        };
        interrupted.push((id, updates));
    }
    Ok(interrupted)
}

fn set_status(
    store: &Store,
    owner: Uuid,
    (project, number): (Uuid, i64),
    (from, to): (IssueStatus, IssueStatus),
    key: String,
    note: String,
) -> bool {
    let written = (|| -> Result<bool> {
        // #2472: a rotated portfolio seat writes through its node's current seat.
        let owner = crate::topology::agent::live_owner(store, owner)?;
        let Some(issue) = store.get_issue_by_display_number(project, number)? else {
            return Ok(false);
        };
        if issue.archived_at.is_some() || issue.status != from {
            return Ok(false);
        }
        store.agent_update_issue_status(
            owner,
            &AgentUpdateIssueStatusRequestV1 {
                issue_id: Some(issue.id),
                display_number: None,
                status: to,
                expected_row_version: issue.row_version,
                idempotency_key: key,
                include_issue: false,
                note: Some(note),
                project_id: Some(project),
            },
        )?;
        Ok(true)
    })();
    match written {
        Ok(done) => done,
        Err(error) => {
            tracing::warn!(%error, issue = number, "topology Issue status write skipped");
            false
        }
    }
}

/// `Open` -> `InProgress` with a note naming the execution, as its owner.
pub(crate) fn mark_in_progress(
    store: &Store,
    owner: Uuid,
    inputs: &serde_json::Value,
    execution_id: Uuid,
) {
    if let Some((project, number)) = crate::topology::starters::issue_snapshot(Some(inputs)) {
        set_status(
            store,
            owner,
            (project, number),
            (IssueStatus::Open, IssueStatus::InProgress),
            format!("topology-start:{execution_id}"),
            format!("topology execution {execution_id} started on this Issue"),
        );
    }
}

/// `InProgress` -> `Open` when `execution` ended without closing its Issue and
/// nothing else holds it. Returns whether the Issue was released.
pub(crate) fn release(
    store: &Store,
    execution: &rows::ExecutionRow,
    status: ExecutionStatus,
) -> bool {
    if !matches!(status, ExecutionStatus::Failed | ExecutionStatus::Cancelled) {
        return false;
    }
    let (Some(owner), Some((project, number))) = (
        execution.requested_by_session_id,
        crate::topology::starters::issue_snapshot(execution.input.as_ref()),
    ) else {
        return false;
    };
    let others = store
        .live_topology_executions_for_issue(project, number)
        .map(|live| live.iter().any(|(id, _, _)| *id != execution.id));
    let worker = store
        .get_issue_by_display_number(project, number)
        .and_then(|issue| match issue {
            Some(issue) => store.live_issue_worker_holder(issue.id),
            None => Ok(None),
        })
        .map(|holder| holder.is_some());
    if !matches!((others, worker), (Ok(false), Ok(false))) {
        return false;
    }
    set_status(
        store,
        owner,
        (project, number),
        (IssueStatus::InProgress, IssueStatus::Open),
        format!("topology-release:{}", execution.id),
        format!(
            "topology execution {} ended {} without closing this Issue; returned to Open",
            execution.id,
            status.as_str()
        ),
    )
}
