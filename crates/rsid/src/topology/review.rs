//! Durable driving of review nodes (#1641 S1a).
//!
//! A review attempt asks the review service for an assignment on the commit
//! its fork points at, records the assignment, and parks in `waiting`; no
//! agent turn of the topology's own runs. Every later round re-derives the
//! route from the service's current verdict (`NodeEffects::review_status`),
//! so a restart never trusts a cached verdict. The verdict becomes the typed
//! attempt output that `verdict_accepted` / `verdict_changes_requested` edges
//! read; the fix node receives the findings through the ordinary upstream
//! rendering.
//!
//! Settlement:
//! - accepted for the forked commit: `succeeded`, verdict `accepted`;
//! - changes requested: `succeeded`, verdict `changes_requested` (findings
//!   attached), unless this was the node's last round (its `max_rounds`, or
//!   the loop's own bound): `blocked(review_rounds_exhausted)`;
//! - no usable verdict (request refused, superseded, wrong commit, a reviewer
//!   that submitted `blocked`): the first time `failed(review_unsettled)` earns
//!   one new assignment; the second time `blocked(review_unsettled)`;
//! - a receipt whose only blocking findings report that a check could not run
//!   (`infra_*` finding keys, #2570): `failed(review_infrastructure)`, an
//!   infrastructure loss retried with a fresh review up to
//!   [`REVIEW_INFRASTRUCTURE_RETRIES`] times. It is no verdict on the code,
//!   spends no review round and is not charged against the attempt cap.
//!
//! - a `blocked` receipt from a reviewer a hub deploy cut off
//!   (`interrupt_workers`, #2809): the same infrastructure class and budget,
//!   with the reason `manager_review_reviewer_interrupted_by_deploy` recorded
//!   in the attempt's error. The review ledger does not count that receipt as
//!   a spent round, so the retry is never refused `review_round_unavailable`.
//!
//! Settling `review_unsettled` also stops the assignment's reviewer if its
//! session is still alive (#2423: an immutable `blocked` receipt does not end
//! the session) and records the interruption in the attempt's error.
//!
//! The new assignment is a fresh review (#2280). It runs under the review
//! ledger's own round policy (`NodeEffects::review_extra_round`: the closing
//! round needs a reviewer no earlier round used), so the ledger never refuses
//! it after the fact, and it waits out [`REVIEW_CAPACITY_BACKOFF`] first when
//! the failure looks like host capacity. A blocked review is resolved with
//! `AgentTopologyResolveAttempt` `retry` (another fresh review of the same
//! commit) or `accept` (the owning manager's recorded ruling, see
//! `resolve.rs`); there is nothing to `discard`.

use rsi_common::harness_manager_v2::{REVIEW_INFRASTRUCTURE_REASON, REVIEW_INTERRUPTED_REASON};
use rsi_common::types::ReviewerLaunch;
use rsi_graph::data::Value as GraphValue;
use uuid::Uuid;

use crate::error::Result;
use crate::topology::executor::{Executor, NodeEffects, failed, node_data};
use crate::topology::graph::{GraphShape, RegionDecision};
use crate::topology::land::GateReceipt;
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, Settlement, failure,
};

/// Marker the review service uses when no manager ledger can own a review.
const REVIEW_NO_MANAGER_LEDGER: &str = "review_no_manager_ledger";

/// Largest findings text recorded in an attempt error.
const FINDINGS_ERROR_MAX: usize = 1_500;

/// How long an automatic review retry waits after a capacity-shaped failure
/// (a reviewer that ended `blocked`, usually because build admission timed out
/// on a busy host) before it asks for a new reviewer (#2280). The retry must
/// not spend its budget in the same instant the host was found full.
pub(crate) const REVIEW_CAPACITY_BACKOFF: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

/// The unsettled reason of a reviewer that submitted a `blocked` receipt.
const REVIEW_BLOCKED_REASON: &str = "manager_review_blocked";

/// How many fresh reviews follow a receipt that only reported unavailable
/// infrastructure before the node blocks for a ruling (#2570).
pub(crate) const REVIEW_INFRASTRUCTURE_RETRIES: usize = 2;

/// Whether an unsettled reason looks like host capacity rather than a verdict
/// problem: the reviewer could not finish its required checks, or admission
/// timed out.
fn capacity_shaped(reason: &str) -> bool {
    reason.contains(REVIEW_BLOCKED_REASON)
        || reason.contains(REVIEW_INFRASTRUCTURE_REASON)
        || reason.contains("admission")
        || reason.contains("capacity")
}

/// Whether an unsettled reason is the reviewer's allocation launch failing
/// (#3640): the launch action ended with the catch-all
/// `manager_v2_lifecycle_unconfirmed` (its original error text follows the code),
/// or settled with no reviewer session. The node reports `review_launch_refused`
/// with that reason, not a bare `review_unsettled`.
fn is_launch_refusal(reason: &str) -> bool {
    reason.starts_with("manager_v2_lifecycle_unconfirmed")
        || reason.starts_with("manager_review_reviewer_unavailable_launch_")
}

/// One review request: everything the review service needs to open an
/// assignment for the commit under review.
#[derive(Clone, Debug)]
pub(crate) struct ReviewRequest {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) node_id: String,
    /// The node whose commit is reviewed.
    pub(crate) of_node: String,
    /// 1-based review round of this node (its loop iteration plus one).
    pub(crate) round: u32,
    /// The commit the review attempt forked from: the commit under review.
    pub(crate) source_commit: String,
    pub(crate) reviewer: ReviewerLaunch,
    pub(crate) execution_name: String,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) epic_id: Option<Uuid>,
    pub(crate) repo_root: std::path::PathBuf,
    /// The ref that pins `source_commit` in `repo_root`.
    pub(crate) pin_ref: String,
    /// The session that authored the first reviewed commit.
    pub(crate) author_session_id: Uuid,
    /// Sessions that authored later (fix) commits under this review node.
    pub(crate) extra_contributors: Vec<Uuid>,
    /// The previous round's assignment when this round re-reviews a fix.
    pub(crate) previous_assignment: Option<Uuid>,
    /// What the reviewer is told to check, in the author's own words.
    pub(crate) query: String,
}

/// The question "may this review node run one more round?" (#1715).
#[derive(Clone, Debug)]
pub(crate) struct ExtraRoundRequest {
    pub(crate) execution_id: Uuid,
    pub(crate) node_id: String,
    pub(crate) project_id: Uuid,
    /// The commit the extra round would review.
    pub(crate) source_commit: String,
    /// The node's configured reviewer.
    pub(crate) reviewer: ReviewerLaunch,
    pub(crate) author_session_id: Uuid,
    pub(crate) extra_contributors: Vec<Uuid>,
}

/// The owning manager's on-call `accept` ruling on an exhausted review (#1740):
/// the review-ledger fact that lets the land node land exactly `commit`.
#[derive(Clone, Debug)]
pub(crate) struct OncallAcceptance {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) node_id: String,
    /// The exhausted review's recorded assignment.
    pub(crate) assignment_id: Uuid,
    /// The commit the exhausted review examined: the only one admitted.
    pub(crate) commit: String,
    pub(crate) decision_key: String,
}

/// What one more review round of an exhausted review node may be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExtraRound {
    /// The node's own reviewer may run it.
    Same,
    /// The store's closure rule applies: the round must use this reviewer.
    Closure(ReviewerLaunch),
    /// No policy-valid round exists; a short machine reason.
    Unavailable(String),
}

/// What the review service reports for one assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewStatus {
    /// The reviewer has not finished.
    Pending,
    /// Accepted for `reviewed_commit`.
    Accepted {
        findings: Vec<String>,
        reviewed_commit: String,
    },
    /// The reviewer asked for changes.
    ChangesRequested { findings: Vec<String> },
    /// No usable verdict will arrive (superseded, reviewer ended without a
    /// receipt, projection mismatch); carries a short machine reason.
    Unsettled(String),
}

/// Typed attempt output of a review node.
pub(crate) fn review_output(
    request_round: u32,
    assignment: Uuid,
    verdict: &str,
    reviewed_commit: &str,
    reviewer: &str,
    findings: &[String],
) -> serde_json::Value {
    let mut fields = serde_json::Map::new();
    fields.insert("verdict".into(), verdict.into());
    fields.insert("reviewed_commit".into(), reviewed_commit.into());
    fields.insert("round".into(), request_round.into());
    fields.insert("assignment_id".into(), assignment.to_string().into());
    fields.insert("reviewer_model".into(), reviewer.into());
    fields.insert(
        "findings".into(),
        serde_json::Value::Array(findings.iter().map(|f| f.as_str().into()).collect()),
    );
    serde_json::to_value(node_data(&serde_json::Value::Object(fields)))
        .unwrap_or(serde_json::Value::Null)
}

fn findings_text(findings: &[String]) -> String {
    let mut text = findings.join("; ");
    if text.len() > FINDINGS_ERROR_MAX {
        let mut end = FINDINGS_ERROR_MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Numbered rendering of a review's `findings` list for the fix node's prompt.
pub(crate) fn render_findings(items: &[GraphValue]) -> String {
    let mut lines = vec!["findings:".to_owned()];
    for (index, item) in items.iter().enumerate() {
        let text = match item {
            GraphValue::String(text) => text.clone(),
            other => other.to_string(),
        };
        lines.push(format!("{}. {text}", index + 1));
    }
    lines.join("\n")
}

impl<E: NodeEffects> Executor<E> {
    /// Whether one more round of the exhausted review `attempt` is policy-valid
    /// and, if the closing-round rule applies, which reviewer it must use.
    pub(crate) async fn review_extra_round(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<ExtraRound> {
        let Some(project_id) = execution.project_id else {
            return Ok(ExtraRound::Unavailable(REVIEW_NO_MANAGER_LEDGER.into()));
        };
        let (_, reviewer, _, _) = Self::review_spec(execution, &attempt.node_id)?;
        let attempts = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
        };
        let context = match review_context(execution, attempt, &attempts) {
            Ok(context) => context,
            Err(reason) => return Ok(ExtraRound::Unavailable(reason)),
        };
        Ok(self
            .effects
            .review_extra_round(ExtraRoundRequest {
                execution_id: execution.id,
                node_id: attempt.node_id.clone(),
                project_id,
                source_commit: attempt.base_commit.clone(),
                reviewer,
                author_session_id: context.author,
                extra_contributors: context.extra_contributors,
            })
            .await)
    }

    /// The round policy for a new review after one that gave no usable verdict
    /// (#2280). An execution with no project has no review ledger to ask (its
    /// review request is refused outright), so nothing is pre-checked. A node
    /// refused before its first assignment has not opened a round either
    /// (#3784); its first reservation checks the live ledger and policy.
    pub(crate) async fn review_retry_round(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<ExtraRound> {
        if execution.project_id.is_none() {
            return Ok(ExtraRound::Same);
        }
        let has_assignment = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
                .iter()
                .any(|other| {
                    other.node_id == attempt.node_id && other.review_assignment_id.is_some()
                })
        };
        if !has_assignment {
            return Ok(ExtraRound::Same);
        }
        self.review_extra_round(execution, attempt).await
    }

    pub(crate) async fn drive_review(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        if execution.status == ExecutionStatus::Cancelling
            && matches!(
                attempt.status,
                AttemptStatus::Reserved | AttemptStatus::Launching | AttemptStatus::Waiting
            )
        {
            // Abandon the wait; a verdict that arrives later is ignored.
            if let Some(key) = super::decision::marker_key(attempt) {
                self.withdraw_decision(execution, key, "execution cancelled")
                    .await?;
            }
            self.settle(
                execution,
                attempt,
                AttemptStatus::Cancelled,
                Settlement {
                    failure_class: Some(failure::CANCELLED),
                    ..Settlement::default()
                },
            )
            .await?;
            return Ok(true);
        }
        match attempt.status {
            AttemptStatus::Reserved | AttemptStatus::Launching => {
                self.request_review_for(execution, attempt).await
            }
            AttemptStatus::Waiting => match super::decision::marker_key(attempt) {
                // The review ran out of rounds and waits for a ruling.
                Some(key) => {
                    let key = key.to_owned();
                    self.drive_review_decision(execution, attempt, &key).await
                }
                None => self.observe_review(execution, attempt).await,
            },
            _ => Ok(false),
        }
    }

    pub(super) fn review_spec(
        execution: &ExecutionRow,
        node: &str,
    ) -> Result<(String, ReviewerLaunch, u8, GraphShape)> {
        let shape = GraphShape::from_workflow(&execution.definition)?;
        match shape.steps().step(node) {
            Some(rsi_common::types::TopologyStep::Review {
                of,
                reviewer,
                max_rounds,
            }) => Ok((of.clone(), reviewer.clone(), *max_rounds, shape)),
            _ => Err(crate::error::DaemonError::InvalidParam(format!(
                "node {node} is not a review node"
            ))),
        }
    }

    async fn request_review_for(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let (of_node, reviewer, _, _) = Self::review_spec(execution, &attempt.node_id)?;
        // #633 (plan §5.3): an agent-requested review re-checks live manager
        // policy before it asks for a reviewer, like a session launch.
        if execution.agent_requested() {
            let refusal = {
                let store = self.store.lock().await;
                crate::topology::agent::launch_gate(&store, execution, attempt)?
            };
            if let Some(code) = refusal {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    failed(failure::POLICY_REFUSED, &code.error_text()),
                )
                .await?;
                return Ok(true);
            }
        }
        let attempts = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
        };
        if let Some(scope) = super::repair_review::scope(execution, attempt, &attempts) {
            let (repo, base, head) = (
                execution.repo_root.clone(),
                scope.reviewed.to_owned(),
                attempt.base_commit.clone(),
            );
            let only_merges = tokio::task::spawn_blocking(move || {
                super::custody::only_rolling_merges(&repo, &base, &head)
            })
            .await
            .unwrap_or(false);
            if only_merges
                && matches!(self.effects.review_status(scope.assignment).await,
                ReviewStatus::Accepted { reviewed_commit, .. } if reviewed_commit == scope.reviewed)
            {
                // Reuse the durable acceptance, never invent a review receipt.
                // observe_review and queue admission independently re-prove it.
                let update = {
                    let store = self.store.lock().await;
                    rows::mark_waiting(
                        &store,
                        execution.id,
                        attempt,
                        scope.assignment,
                        self.boot_id,
                    )?
                };
                self.publish([update]);
                return Ok(true);
            }
        }
        let context = match review_context(execution, attempt, &attempts) {
            Ok(context) => context,
            Err(reason) => {
                self.settle_unsettled(execution, attempt, &reason).await?;
                return Ok(true);
            }
        };
        // An automatic retry of a review that gave no usable verdict waits out
        // a capacity-shaped failure first (#2280).
        if self
            .review_retry_backoff_pending(&attempts, attempt)
            .await?
        {
            return Ok(false);
        }
        // The reopened round of an exhausted review, and the new review of one
        // that gave no usable verdict, run under the store's own round policy:
        // each may need a different (closing) reviewer, and a reviewer the
        // ledger would refuse is never requested (#2280).
        let same_instance = |other: &&AttemptRow, class: &str| {
            other.node_id == attempt.node_id
                && other.iteration == attempt.iteration
                && other.failure_class.as_deref() == Some(class)
        };
        let reopened = attempts
            .iter()
            .any(|other| same_instance(&other, failure::REVIEW_REOPENED));
        let retried = attempts.iter().any(|other| {
            same_instance(&other, failure::REVIEW_UNSETTLED)
                || same_instance(&other, failure::REVIEW_LAUNCH_REFUSED)
        });
        let round = if reopened {
            Some(self.review_extra_round(execution, attempt).await?)
        } else if retried {
            Some(self.review_retry_round(execution, attempt).await?)
        } else {
            None
        };
        let reviewer = match round {
            None | Some(ExtraRound::Same) => reviewer,
            Some(ExtraRound::Closure(closing)) => closing,
            Some(ExtraRound::Unavailable(reason)) => {
                let (class, text) = if reopened {
                    (
                        failure::REVIEW_ROUNDS_EXHAUSTED,
                        format!("the extra review round is unavailable: {reason}"),
                    )
                } else {
                    (
                        failure::REVIEW_UNSETTLED,
                        format!("the review retry round is unavailable: {reason}"),
                    )
                };
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    failed(class, &text),
                )
                .await?;
                return Ok(true);
            }
        };
        let mut query = context.query;
        if let Some(producer) = producer(&attempts, &attempt.base_commit) {
            let store = self.store.lock().await;
            query.push_str(&super::review_receipts::source_receipt_text(
                &store,
                execution,
                producer,
                &attempt.base_commit,
            )?);
        }
        // #2570: a published commit already passed the merge-queue gate; show
        // the reviewer that receipt so it does not re-run the gate's tests.
        if let Some(entry) = published_land_entry(&attempts, &attempt.base_commit)
            && let Some(gate) = self.effects.land_gate_receipt(entry).await
        {
            query.push_str(&gate_receipt_text(&attempt.base_commit, entry, &gate));
        }
        let request = ReviewRequest {
            execution_id: execution.id,
            attempt_id: attempt.id,
            node_id: attempt.node_id.clone(),
            of_node,
            round: attempt.iteration.saturating_add(1),
            source_commit: attempt.base_commit.clone(),
            reviewer,
            execution_name: execution.name.clone(),
            project_id: execution.project_id,
            epic_id: execution.epic_id,
            repo_root: execution.repo_root.clone(),
            pin_ref: context.pin_ref,
            author_session_id: context.author,
            extra_contributors: context.extra_contributors,
            previous_assignment: context.previous_assignment,
            query,
        };
        tracing::debug!(
            execution_id = %request.execution_id,
            node_id = %request.node_id,
            of_node = %request.of_node,
            round = request.round,
            "topology review requested"
        );
        match self.effects.request_review(request).await {
            Ok(assignment) => {
                let update = {
                    let store = self.store.lock().await;
                    rows::mark_waiting(&store, execution.id, attempt, assignment, self.boot_id)?
                };
                self.publish([update]);
            }
            Err(error) if error.to_string().contains(REVIEW_NO_MANAGER_LEDGER) => {
                // No live project-manager ledger owns this review: retrying
                // the request cannot help, so the node blocks visibly at once.
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    failed(failure::REVIEW_NO_MANAGER_LEDGER, &error.to_string()),
                )
                .await?;
            }
            Err(error) => {
                self.settle_unsettled(execution, attempt, &error.to_string())
                    .await?;
            }
        }
        Ok(true)
    }

    /// First `review_unsettled` of an instance fails (and is retried with a
    /// new assignment); the next one blocks.
    async fn settle_unsettled(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        reason: &str,
    ) -> Result<()> {
        // #2809: a reviewer a hub deploy cut off gave no verdict either; it
        // shares the infrastructure budget and spends no review round.
        let infrastructure =
            reason == REVIEW_INFRASTRUCTURE_REASON || reason == REVIEW_INTERRUPTED_REASON;
        // #3640: a reviewer whose launch never produced a session is a launch
        // problem; it keeps the same budget but names the cause.
        let unsettled_class = if is_launch_refusal(reason) {
            failure::REVIEW_LAUNCH_REFUSED
        } else {
            failure::REVIEW_UNSETTLED
        };
        let prior = {
            let class = if infrastructure {
                failure::REVIEW_INFRASTRUCTURE
            } else {
                unsettled_class
            };
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
                .iter()
                .filter(|other| {
                    other.node_id == attempt.node_id
                        && other.iteration == attempt.iteration
                        && other.id != attempt.id
                        && if infrastructure {
                            other.failure_class.as_deref() == Some(class)
                        } else {
                            failure::is_review_unsettled(other.failure_class.as_deref())
                        }
                })
                .count()
        };
        // A reviewer that reported unavailable infrastructure earns its own,
        // larger retry budget; once spent the node blocks like any other
        // review that cannot settle (#2570).
        let (status, class) = if infrastructure && prior < REVIEW_INFRASTRUCTURE_RETRIES {
            (AttemptStatus::Failed, failure::REVIEW_INFRASTRUCTURE)
        } else if !infrastructure && prior == 0 {
            (AttemptStatus::Failed, unsettled_class)
        } else {
            (AttemptStatus::Blocked, unsettled_class)
        };
        // #2423: a reviewer can outlive its verdict (an immutable `blocked`
        // receipt does not end the session). The topology has given up on this
        // assignment, so stop the reviewer and record that beside the reason.
        let stopped = match attempt.review_assignment_id {
            Some(assignment) => self.effects.stop_reviewer(assignment).await,
            None => None,
        };
        let recorded = match stopped {
            Some(session) => format!("{reason} (reviewer session {session} interrupted)"),
            None => reason.to_owned(),
        };
        self.settle(execution, attempt, status, failed(class, &recorded))
            .await
    }

    /// Whether `attempt` is an automatic retry still inside the backoff that
    /// follows a capacity-shaped `review_unsettled` (#2280). A retry the
    /// operator or manager asked for (`resolution` set on its predecessor) is
    /// never delayed.
    async fn review_retry_backoff_pending(
        &self,
        attempts: &[AttemptRow],
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let Some(previous) = attempts.iter().find(|other| {
            other.node_id == attempt.node_id
                && other.iteration == attempt.iteration
                && other.attempt_no.saturating_add(1) == attempt.attempt_no
        }) else {
            return Ok(false);
        };
        if !matches!(
            previous.failure_class.as_deref(),
            Some(
                failure::REVIEW_UNSETTLED
                    | failure::REVIEW_LAUNCH_REFUSED
                    | failure::REVIEW_INFRASTRUCTURE
            )
        ) || previous.resolution.is_some()
            || !capacity_shaped(previous.error.as_deref().unwrap_or_default())
        {
            return Ok(false);
        }
        let finished = {
            let store = self.store.lock().await;
            rows::attempt_finished_at(&store, previous.id)?
        };
        Ok(
            finished
                .is_some_and(|finished| chrono::Utc::now() < finished + REVIEW_CAPACITY_BACKOFF),
        )
    }

    async fn observe_review(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        let Some(assignment) = attempt.review_assignment_id else {
            self.settle_unsettled(execution, attempt, "waiting review has no assignment")
                .await?;
            return Ok(true);
        };
        let (_, reviewer, max_rounds, shape) = Self::review_spec(execution, &attempt.node_id)?;
        let round = attempt.iteration.saturating_add(1);
        match self.effects.review_status(assignment).await {
            ReviewStatus::Pending => self.review_wait_bound(execution, attempt).await,
            ReviewStatus::Unsettled(reason) => {
                self.settle_unsettled(execution, attempt, &reason).await?;
                Ok(true)
            }
            ReviewStatus::Accepted {
                findings,
                reviewed_commit,
            } => {
                let mut rolling_merge_base = None;
                if reviewed_commit != attempt.base_commit {
                    let attempts = {
                        let store = self.store.lock().await;
                        rows::load_attempts(&store, execution.id)?
                    };
                    if let Some(scope) = super::repair_review::scope(execution, attempt, &attempts)
                        && scope.assignment == assignment
                        && scope.reviewed == reviewed_commit
                    {
                        let (repo, base, head) = (
                            execution.repo_root.clone(),
                            reviewed_commit.clone(),
                            attempt.base_commit.clone(),
                        );
                        if tokio::task::spawn_blocking(move || {
                            super::custody::only_rolling_merges(&repo, &base, &head)
                        })
                        .await
                        .unwrap_or(false)
                        {
                            rolling_merge_base = Some(reviewed_commit.clone());
                        }
                    }
                }
                if reviewed_commit != attempt.base_commit && rolling_merge_base.is_none() {
                    self.settle_unsettled(
                        execution,
                        attempt,
                        "accepted commit differs from the commit under review",
                    )
                    .await?;
                    return Ok(true);
                }
                let mut output = review_output(
                    round,
                    assignment,
                    "accepted",
                    &attempt.base_commit,
                    &reviewer.model,
                    &findings,
                );
                if let Some(base) = rolling_merge_base {
                    output["fields"]["rolling_merge_base"] = base.into();
                    output["fields"]["review_skipped"] = "only_rolling_merges".into();
                }
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Succeeded,
                    Settlement {
                        result_commit: Some(attempt.base_commit.clone()),
                        output: Some(output),
                        ..Settlement::default()
                    },
                )
                .await?;
                Ok(true)
            }
            ReviewStatus::ChangesRequested { findings } => {
                let output = review_output(
                    round,
                    assignment,
                    "changes_requested",
                    &attempt.base_commit,
                    &reviewer.model,
                    &findings,
                );
                let loop_halts = shape.region_of(&attempt.node_id).is_some_and(|region| {
                    matches!(
                        shape.decide_region(region, attempt.iteration, false, false),
                        RegionDecision::Halt(_)
                    )
                });
                if round >= u32::from(max_rounds) || loop_halts {
                    let error = format!(
                        "changes still requested after round {round}/{max_rounds}: {}",
                        findings_text(&findings)
                    );
                    // #1641 S3c: the on-call manager decides what an exhausted
                    // review means; without one the node blocks as before.
                    if self
                        .park_review_exhaustion(execution, attempt, &output, &error)
                        .await?
                    {
                        return Ok(true);
                    }
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Blocked,
                        Settlement {
                            failure_class: Some(failure::REVIEW_ROUNDS_EXHAUSTED),
                            error: Some(error),
                            output: Some(output),
                            ..Settlement::default()
                        },
                    )
                    .await?;
                } else {
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Succeeded,
                        Settlement {
                            result_commit: Some(attempt.base_commit.clone()),
                            output: Some(output),
                            ..Settlement::default()
                        },
                    )
                    .await?;
                }
                Ok(true)
            }
        }
    }

    /// A review that never answers fails at the node wall time like any
    /// other node, so a lost reviewer cannot hold an execution forever.
    async fn review_wait_bound(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        if !crate::topology::executor::wall_time_expired(execution, attempt) {
            return Ok(false);
        }
        self.settle(
            execution,
            attempt,
            AttemptStatus::Failed,
            failed(failure::TIMEOUT, "review exceeded the node wall-time limit"),
        )
        .await?;
        Ok(true)
    }
}

/// #2677: where a topology reviewer reports a finding. One finding becomes
/// exactly one Issue, so the reviewer never files it: the receipt carries it and
/// the topology (the post-land follow-up node) files and dedupes the Issues.
/// A process problem about the review itself is still a kaizen Issue.
const REVIEW_FINDINGS_RULE: &str = "\n\nFindings: report every finding in your receipt only. Do \
not file a finding as an Issue (no AgentCreateIssue for a code or test defect): the topology turns \
the receipt findings into deduped follow-up Issues, and a second Issue for the same finding is a \
duplicate. A kaizen Issue about the review process itself is still allowed.";

/// What every topology reviewer is told about tests (#2570): the verified
/// receipts are the test evidence, a reviewer reviews code, and a check that
/// cannot run is reported as infrastructure instead of as a code finding.
const REVIEW_TEST_EVIDENCE_RULES: &str = "\n\nTest evidence: the author's verified `scoped_test` \
    receipt snapshot attached below (and, for a published commit, the merge-queue gate receipt) is the test evidence for \
    this change. Do not re-run those tests and do not run `scripts/scoped-test`: a run waits for a \
    build slot and proves nothing the receipt does not. Read the snapshot directly; AgentGetJob \
    and AgentListJobs remain own-job only. For a pre-merge review, only trust `trust: verified` or \
    `trust: trusted_with_baseline_reds` for this exact source. `trusted_with_baseline_reds` is a \
    red run whose every failure is a tracked baseline red (the snapshot's `baseline_reds` lists each \
    test with its open Issue) and the daemon found no new failure: it is accepted verification, \
    never a finding, so do not block on those reds or on the job's non-zero exit. \
    `untrusted` or `absent` does not meet the test minimum and must remain blocked with an \
    `infra_` finding. For a published source, the merge-queue gate receipt remains test evidence. \
    Do not substitute an author's self-reported receipt. You may submit at most one exact-name test \
    job to prove one specific finding. Your reviewer sandbox is pinned to the exact reviewed source \
    commit, so that job runs the source's tests. Make no changes or commits. If a check you need cannot run (no build slot, a missing \
    runner), do not file it as a code finding: submit verdict `blocked` with a blocking finding \
    whose key starts with `infra_` (for example `infra_tests_unavailable`). The topology treats \
    that as an infrastructure retry, not as a verdict on the code, and it spends no review round.";

/// Longest filter list quoted from a gate receipt.
const GATE_FILTERS_MAX: usize = 1_500;

/// The post-land reviewer's view of the gate that already tested the source
/// (#2570): the filters it ran, that the entry published, and where the log is.
fn gate_receipt_text(commit: &str, entry: Uuid, gate: &GateReceipt) -> String {
    let filters = if gate.test_filters.is_empty() {
        "no focused filters were recorded for it".to_owned()
    } else {
        format!(
            "it ran {} derived filter(s): {}",
            gate.test_filters.len(),
            clip(&gate.test_filters.join(", "), GATE_FILTERS_MAX)
        )
    };
    let log = gate.log_glob.as_deref().map_or_else(String::new, |glob| {
        format!(" The gate log with each test's result is `{glob}`.")
    });
    format!(
        "\n\nMerge-queue gate receipt for source {commit} (queue entry {entry}, state {}): \
         {filters}. The entry published, so those tests passed.{log} Do not re-run what the gate \
         selected; run only an exact-name test the gate did not select, and only to prove a \
         specific finding.",
        gate.state
    )
}

/// The queue entry of the land node that published `commit` in this execution.
fn published_land_entry(attempts: &[AttemptRow], commit: &str) -> Option<Uuid> {
    attempts
        .iter()
        .filter(|other| {
            other.node_kind == "land"
                && other.status == AttemptStatus::Succeeded
                && other.base_commit == commit
        })
        .filter_map(|land| land.land_entry_id)
        .next_back()
}

/// Longest author task text quoted to the reviewer.
const AUTHOR_QUERY_MAX: usize = 4_000;

/// What the executor knows about the commit a review attempt examines.
struct ReviewContext {
    /// The session that authored the first reviewed commit.
    author: Uuid,
    /// Sessions that authored later fix commits under this review node.
    extra_contributors: Vec<Uuid>,
    /// The previous round's assignment, when that round asked for changes.
    previous_assignment: Option<Uuid>,
    /// The ref that pins the commit under review.
    pin_ref: String,
    /// The reviewer's request text.
    query: String,
}

/// The published SHA of the merge-queue landing of `commit` in this execution,
/// when a land node has already published it (#2565).
fn published_sha<'a>(attempts: &'a [AttemptRow], commit: &str) -> Option<&'a str> {
    attempts
        .iter()
        .filter(|other| {
            other.node_kind == "land"
                && other.status == AttemptStatus::Succeeded
                && other.base_commit == commit
        })
        .filter_map(|land| {
            land.output
                .as_ref()?
                .pointer("/fields/landed_sha")?
                .as_str()
        })
        .next_back()
}

/// The session attempt that produced `commit` in this execution.
pub(crate) fn producer<'a>(attempts: &'a [AttemptRow], commit: &str) -> Option<&'a AttemptRow> {
    attempts.iter().rev().find(|other| {
        other.node_kind == "session"
            && other.status == AttemptStatus::Succeeded
            && other.result_commit.as_deref() == Some(commit)
    })
}

pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Resolve who authored the commit under review, which fix sessions also
/// touched it, what the previous round found, and where the commit is pinned.
/// A reason string means the commit has no known producer in this execution.
fn review_context(
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    attempts: &[AttemptRow],
) -> std::result::Result<ReviewContext, String> {
    let mut reviews: Vec<&AttemptRow> = attempts
        .iter()
        .filter(|other| other.node_kind == "review" && other.node_id == attempt.node_id)
        .collect();
    reviews.sort_by_key(|other| (other.iteration, other.attempt_no));
    let first = reviews
        .first()
        .map_or(attempt.base_commit.as_str(), |first| {
            first.base_commit.as_str()
        });
    let author = producer(attempts, first)
        .ok_or_else(|| format!("no node of this execution produced reviewed commit {first}"))?;
    let current = producer(attempts, &attempt.base_commit).ok_or_else(|| {
        format!(
            "no node of this execution produced reviewed commit {}",
            attempt.base_commit
        )
    })?;
    let mut extra_contributors: Vec<Uuid> = Vec::new();
    for commit in reviews
        .iter()
        .map(|review| review.base_commit.as_str())
        .chain(std::iter::once(attempt.base_commit.as_str()))
    {
        if let Some(other) = producer(attempts, commit)
            && other.session_id != author.session_id
            && !extra_contributors.contains(&other.session_id)
        {
            extra_contributors.push(other.session_id);
        }
    }
    let previous_assignment = reviews
        .iter()
        .rev()
        .filter(|review| review.iteration < attempt.iteration)
        .find(|review| {
            review.review_assignment_id.is_some()
                && review
                    .output
                    .as_ref()
                    .and_then(|output| output.pointer("/fields/verdict"))
                    .and_then(serde_json::Value::as_str)
                    == Some("changes_requested")
        })
        .and_then(|review| review.review_assignment_id);
    let task = clip(author.query(), AUTHOR_QUERY_MAX);
    let mut query = format!(
        "Independent review of commit {commit} from topology execution `{name}` (review node \
         `{node}`, round {round}). The commit was produced by node `{producer}` for this task:\n\n\
         {task}\n\nCheck the change against that task: correctness, tests that assert the stated \
         intent, regressions, and repository rules. A finding that blocks acceptance must be \
         marked blocking.",
        commit = attempt.base_commit,
        name = execution.name,
        node = attempt.node_id,
        round = attempt.iteration.saturating_add(1),
        producer = current.node_id,
    );
    // #2565: a post-land review reads work that is already published. Name
    // both identities so the reviewer examines what is on rolling, and so its
    // findings are understood as follow-ups rather than a veto.
    if let Some(published) = published_sha(attempts, &attempt.base_commit) {
        query.push_str(&format!(
            "\n\nThis commit has already been published to rolling as {published} (the merge \
             queue's published commit for source commit {commit}). Review the published change; \
             each real defect you report in your receipt becomes a follow-up Issue that the \
             topology files and dedupes, and does not revert the landing.",
            commit = attempt.base_commit,
        ));
    }
    query.push_str(REVIEW_TEST_EVIDENCE_RULES);
    query.push_str(REVIEW_FINDINGS_RULE);
    if let Some(scope) = super::repair_review::scope(execution, attempt, attempts) {
        query.push_str(&scope.prompt(&attempt.base_commit));
    }
    Ok(ReviewContext {
        author: author.session_id,
        extra_contributors,
        previous_assignment,
        pin_ref: crate::topology::custody::node_pin_ref(
            execution.id,
            &current.node_id,
            current.iteration,
        ),
        query,
    })
}
