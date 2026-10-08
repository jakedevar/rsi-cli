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
//! - no usable verdict (request refused, superseded, wrong commit): the first
//!   time `failed(review_unsettled)` earns one new assignment; the second time
//!   `blocked(review_unsettled)`.

use rsi_common::types::ReviewerLaunch;
use rsi_graph::data::Value as GraphValue;
use uuid::Uuid;

use crate::error::Result;
use crate::topology::executor::{Executor, NodeEffects, failed, node_data};
use crate::topology::graph::{GraphShape, RegionDecision};
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, Settlement, failure,
};

/// Marker the review service uses when no manager ledger can own a review.
const REVIEW_NO_MANAGER_LEDGER: &str = "review_no_manager_ledger";

/// Largest findings text recorded in an attempt error.
const FINDINGS_ERROR_MAX: usize = 1_500;

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
fn review_output(
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

    fn review_spec(
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
                    failed(failure::POLICY_REFUSED, code),
                )
                .await?;
                return Ok(true);
            }
        }
        let attempts = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
        };
        let context = match review_context(execution, attempt, &attempts) {
            Ok(context) => context,
            Err(reason) => {
                self.settle_unsettled(execution, attempt, &reason).await?;
                return Ok(true);
            }
        };
        // The reopened round of an exhausted review runs under the store's
        // own round policy: it may need a different (closing) reviewer.
        let reopened = attempts.iter().any(|other| {
            other.node_id == attempt.node_id
                && other.iteration == attempt.iteration
                && other.failure_class.as_deref() == Some(failure::REVIEW_REOPENED)
        });
        let reviewer = if reopened {
            match self.review_extra_round(execution, attempt).await? {
                ExtraRound::Same => reviewer,
                ExtraRound::Closure(closing) => closing,
                ExtraRound::Unavailable(reason) => {
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Blocked,
                        failed(
                            failure::REVIEW_ROUNDS_EXHAUSTED,
                            &format!("the extra review round is unavailable: {reason}"),
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
            }
        } else {
            reviewer
        };
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
            query: context.query,
        };
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
        let prior = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
                .iter()
                .filter(|other| {
                    other.node_id == attempt.node_id
                        && other.iteration == attempt.iteration
                        && other.id != attempt.id
                        && other.failure_class.as_deref() == Some(failure::REVIEW_UNSETTLED)
                })
                .count()
        };
        let status = if prior == 0 {
            AttemptStatus::Failed
        } else {
            AttemptStatus::Blocked
        };
        self.settle(
            execution,
            attempt,
            status,
            failed(failure::REVIEW_UNSETTLED, reason),
        )
        .await
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
                if reviewed_commit != attempt.base_commit {
                    self.settle_unsettled(
                        execution,
                        attempt,
                        "accepted commit differs from the commit under review",
                    )
                    .await?;
                    return Ok(true);
                }
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Succeeded,
                    Settlement {
                        result_commit: Some(reviewed_commit.clone()),
                        output: Some(review_output(
                            round,
                            assignment,
                            "accepted",
                            &reviewed_commit,
                            &reviewer.model,
                            &findings,
                        )),
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

/// The session attempt that produced `commit` in this execution.
pub(crate) fn producer<'a>(attempts: &'a [AttemptRow], commit: &str) -> Option<&'a AttemptRow> {
    attempts.iter().rev().find(|other| {
        other.node_kind == "session"
            && other.status == AttemptStatus::Succeeded
            && other.result_commit.as_deref() == Some(commit)
    })
}

fn clip(text: &str, max: usize) -> String {
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
    let query = format!(
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
