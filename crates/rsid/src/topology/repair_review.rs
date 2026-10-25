//! Focused review of a repair of an already reviewed landing (#3338).
//!
//! `review_repair_delta=true` opts a review node into this scope. The flag
//! alone grants nothing: the verified producer must repair a failed land
//! whose exact source was accepted by a review in this execution.

use uuid::Uuid;

use super::steps::WorkflowSteps;
use super::store::{AttemptRow, AttemptStatus, ExecutionRow, failure};

pub(crate) struct RepairScope<'a> {
    pub(crate) base: &'a str,
    pub(crate) reviewed: &'a str,
    pub(crate) assignment: Uuid,
    reason: &'a str,
    entry: Option<Uuid>,
}

pub(crate) fn scope<'a>(
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    attempts: &'a [AttemptRow],
) -> Option<RepairScope<'a>> {
    let node = execution
        .definition
        .nodes
        .iter()
        .find(|n| n.id == attempt.node_id)?;
    if !node
        .tags
        .iter()
        .any(|tag| tag == "review_repair_delta=true")
    {
        return None;
    }
    let steps = WorkflowSteps::from_workflow(&execution.definition).ok()?;
    let of = steps.review_of(&attempt.node_id)?;
    let repair = attempts.iter().rev().find(|other| {
        other.node_id == of
            && other.status == AttemptStatus::Succeeded
            && other.result_commit.as_deref() == Some(attempt.base_commit.as_str())
            && steps.require_verified(of)
    })?;
    let land = attempts.iter().rev().find(|other| {
        other.node_kind == "land"
            && other.status == AttemptStatus::Failed
            && other.failure_class.as_deref() == Some(failure::LAND_REFUSED)
            && other.base_commit == repair.base_commit
            && execution.definition.edges.iter().any(|edge| {
                edge.source == other.node_id
                    && edge.target == repair.node_id
                    && steps.when(&edge.source, &edge.target)
                        == rsi_common::types::EdgeWhen::Failure
            })
    })?;
    let Some(rsi_common::types::TopologyStep::Land { accepted, .. }) = steps.step(&land.node_id)
    else {
        return None;
    };
    let review = attempts.iter().rev().find(|other| {
        other.node_id == *accepted
            && other.node_kind == "review"
            && other.status == AttemptStatus::Succeeded
            && other.result_commit.as_deref() == Some(repair.base_commit.as_str())
            && other
                .output
                .as_ref()
                .and_then(|o| o.pointer("/fields/verdict"))
                .and_then(|v| v.as_str())
                == Some("accepted")
    })?;
    let reviewed = review
        .output
        .as_ref()
        .and_then(|o| o.pointer("/fields/rolling_merge_base"))
        .and_then(|v| v.as_str())
        .unwrap_or(&repair.base_commit);
    Some(RepairScope {
        base: &repair.base_commit,
        reviewed,
        assignment: review.review_assignment_id?,
        reason: land.error.as_deref().unwrap_or("land_refused"),
        entry: land.land_entry_id,
    })
}

impl RepairScope<'_> {
    pub(crate) fn prompt(&self, head: &str) -> String {
        format!(
            "\n\nFocused repair review: the change through {base} was already accepted. \
             Review ONLY the repair delta from {base} to {head}, not the original implementation \
             or commits imported from rolling. Use git log --first-parent {base}..{head} and \
             inspect ordinary commit diffs and git show --remerge-diff for merge resolutions. \
             The acceptance is for the exact repaired head {head}.\n\
             Queue entry: {entry}\nRefusal/cancellation to repair: {reason}",
            base = self.base,
            entry = self
                .entry
                .map_or_else(|| "not enqueued".into(), |id| id.to_string()),
            reason = super::review::clip(self.reason, 2_000),
        )
    }
}
