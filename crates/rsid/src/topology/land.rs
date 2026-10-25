//! Durable driving of land nodes (#1641 S2).
//!
//! A land attempt publishes the commit an accepted review (or, #2249, a
//! `require_verified` session's own scoped-test receipt) admitted by
//! enqueuing it on the daemon merge queue on behalf of the execution's owning
//! manager or Epic lead, then parks in `waiting` with the queue entry id
//! recorded. No agent turn runs: every later tick mirrors the entry's state,
//! so a restart never re-submits (the entry id is stored, and the queue's
//! `(source session, replay key)` identity returns the same entry for a
//! duplicate enqueue).
//!
//! Settlement:
//! - the accepted review no longer admits the commit, or the requester lost
//!   authority: `blocked(land_admission_lost)`, nothing enqueued;
//! - a land admitted by verification (no review) whose source carries a
//!   migration: `blocked(land_requires_review)`, nothing enqueued (AGENTS.md
//!   reserves pre-land review for migrations); when the land node has a
//!   `review_required` out-edge (#2688) it settles `failed(land_requires_review)`
//!   instead and that edge routes the exact head to the graph's own pre-land
//!   review node, so the execution never strands;
//! - a policy-bearing execution (an Issue-pipeline starter, #2304 slice 3)
//!   whose candidate source needs a stricter review than its graph provides
//!   (migration, credential/IAM/network, custody or security paths; git
//!   unable to say fails closed): `blocked(land_requires_review)` naming the
//!   required policy and exact source SHA, nothing enqueued (or the routed
//!   `failed(land_requires_review)` above);
//! - the execution is cancelled before the entry exists (including while the
//!   Git probe ran): `cancelled`, nothing enqueued; an entry created before
//!   the cancellation is still adopted and mirrored;
//! - the merge queue is disabled: `blocked(queue_disabled)`, nothing enqueued;
//! - the queue refuses the source (invalid, duplicate, a filter selecting no
//!   test): `failed(land_refused)`;
//! - #3358: an entry replaced in place by a descendant source
//!   (`AgentEnqueueLandingSource supersedes`, which keeps the queue position and
//!   the wake) records its successor in its outcome; the waiting land node
//!   follows the successor (`land_entry_followed` event) and settles on its
//!   outcome, so a repaired descendant needs no repair round. A cancel without
//!   a successor stays `failed(land_refused)`;
//! - the entry ends `published`: `succeeded` with the landed tip in its typed
//!   output; a refused, failed or superseded entry whose source is nonetheless
//!   an ancestor of a freshly fetched `origin/rolling` settles the same way
//!   (#2599: it landed, a repair would have nothing to commit); any other
//!   `refused`, `failed` or `superseded` entry is `failed(land_refused)`,
//!   never retried (a second landing is a decision, not a retry);
//! - #3036: an entry refused `queue_batch_merge_conflict` while a live entry
//!   ahead of it (queued, admitted or gating, enqueued earlier) whose own
//!   change conflicts with its source has not settled keeps the attempt
//!   `waiting` until each such entry publishes or is refused, so the repair it
//!   routes to merges a rolling that already holds that entry instead of
//!   burning an attempt racing it; the entries are chosen once and recorded
//!   (`land_conflict_wait` event);
//! - a published land with `close_issue` first closes the execution's input
//!   Issue as its owner (`issue_close`, #2260). The close is idempotent and
//!   re-run on the next tick after a crash; a refused authority is data in the
//!   output (`issue_close: {status: "refused", code}`), never a failed land,
//!   and a refused, failed or superseded entry never touches the Issue.
//!
//! A queued landing is not bounded by the node wall time: the queue has its
//! own gate budget. An interrupted publication keeps waiting on the same
//! entry while the queue re-admits it at its original FIFO position. Legacy
//! terminal entries stay immutable and follow the ordinary refusal path, with
//! a fresh ancestry check before repair.
//! Failing the node while the entry might still publish would make the record lie.

use std::path::PathBuf;

use uuid::Uuid;

use crate::error::Result;
use crate::topology::executor::{Executor, NodeEffects, failed, node_data, node_output};
use crate::topology::issue_close::{IssueClose, IssueCloseRequest, close_input_issue};
use crate::topology::review_policy::ReviewPolicy;
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, Settlement, failure,
};

/// One landing request: everything the queue admission needs, taken from the
/// execution and the accepted review attempt.
#[derive(Clone, Debug)]
pub(crate) struct LandRequest {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    /// The attempt's dedup key: the queue replay key, so a duplicate enqueue
    /// returns the same entry.
    pub(crate) dedup_key: String,
    pub(crate) project_id: Option<Uuid>,
    pub(crate) epic_id: Option<Uuid>,
    /// Where the node pins live: the commit is reachable here after node
    /// sandboxes are reclaimed.
    pub(crate) repo_root: PathBuf,
    /// The accepted commit.
    pub(crate) source_commit: String,
    /// What accepted `source_commit`.
    pub(crate) acceptance: LandAcceptance,
    /// The accepted author session: the queue entry's source identity.
    pub(crate) author_session_id: Uuid,
    pub(crate) test_filters: Vec<String>,
    /// #2304 slice 3: the review policy this land node's graph covers, when the
    /// execution is policy-bearing (an Issue-pipeline starter, or it passed
    /// `inputs.review_policy`); `None` leaves the topology's behaviour as is.
    pub(crate) review_coverage: Option<ReviewPolicy>,
}

/// What admits a land node's commit: a review's accepted verdict, or (#2249)
/// the receipt of the producing session's own `scoped_test` job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LandAcceptance {
    /// The review node and the assignment that accepted `source_commit`.
    Review { node: String, assignment_id: Uuid },
    /// An accepted source followed only by proven clean merges of rolling.
    /// Queue admission rechecks both the original verdict and the merge proof.
    RollingMerge {
        node: String,
        assignment_id: Uuid,
        reviewed_commit: String,
    },
    /// The `require_verified` session node and the terminal `scoped_test` job
    /// (owned by `session_id`) whose receipt `head` is `source_commit`.
    Verified {
        node: String,
        session_id: Uuid,
        job_id: Uuid,
        head: String,
    },
}

impl LandAcceptance {
    /// Admitted by verification rather than by a reviewer.
    pub(crate) const fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

/// The merge-queue gate's receipt for a published land entry, as a review of
/// the landed source reads it (#2570): the gate is the test evidence for a
/// source that already passed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GateReceipt {
    /// The queue entry's state (`published`).
    pub(crate) state: String,
    /// The filters the gate ran.
    pub(crate) test_filters: Vec<String>,
    /// Where the gate's log lists each test's result, when known.
    pub(crate) log_glob: Option<String>,
}

/// The gate receipt of queue entry `entry_id`, read from the store (#2570,
/// #3020): `None` unless the entry is `published`. The gate log is
/// `<data dir>/queue/logs/<batch>-*.log`.
pub(crate) fn gate_receipt(store: &crate::store::Store, entry_id: Uuid) -> Option<GateReceipt> {
    let entry = store.get_rolling_queue_entry(entry_id).ok().flatten()?;
    if entry.state != rsi_common::rolling_queue::RollingQueueEntryState::Published {
        return None;
    }
    let batch = store.rolling_queue_entry_batch_id(entry_id).ok().flatten();
    let log_glob = batch.map(|batch| {
        rsi_common::identity::data_dir()
            .join("queue")
            .join("logs")
            .join(format!("{batch}-*.log"))
            .display()
            .to_string()
    });
    Some(GateReceipt {
        state: entry.state.as_str().to_owned(),
        test_filters: entry.test_filters,
        log_glob,
    })
}

/// What the merge queue answered to one enqueue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LandEnqueue {
    /// The entry exists (new or replayed).
    Queued(Uuid),
    /// The acceptance or the requester's authority no longer admits the
    /// landing; carries a short machine reason.
    AdmissionLost(String),
    /// The execution was cancelled (or the attempt is no longer the live
    /// one) before any entry was created: nothing was enqueued.
    Cancelled,
    /// The operator disabled the queue.
    QueueDisabled,
    /// A land admitted by verification found a migration in its source: it
    /// needs a pre-land review, so nothing was enqueued (#2249).
    RequiresReview(String),
    /// The queue refused the source; carries its stable `queue_*` code.
    Refused(String),
}

/// A queue entry as a land node mirrors it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LandStatus {
    /// Queued, admitted or gating.
    Pending,
    Published {
        landed_sha: Option<String>,
    },
    /// #3358: replaced in place by a descendant source (`supersedes`), which
    /// kept the queue position and the wake: the land node follows `successor`.
    Superseded {
        successor: Uuid,
    },
    /// Refused, failed, cancelled (or the entry vanished).
    Refused {
        state: String,
        /// Stable refusal code first (for conflict classification), followed
        /// by the bounded queue detail, including any conflicting paths.
        reason: String,
    },
}

impl LandStatus {
    /// The lander reports `already_on_rolling` as published-equivalent. The
    /// queue persists it using the existing published state and containing tip,
    /// so issue closure and post-land review follow the same success path.
    pub(crate) fn from_queue(
        state: rsi_common::rolling_queue::RollingQueueEntryState,
        outcome: Option<rsi_common::rolling_queue::RollingQueueOutcome>,
    ) -> Self {
        use rsi_common::rolling_queue::RollingQueueEntryState;
        match state {
            RollingQueueEntryState::Queued
            | RollingQueueEntryState::Admitted
            | RollingQueueEntryState::Gating => Self::Pending,
            RollingQueueEntryState::Published => Self::Published {
                landed_sha: outcome.and_then(|outcome| outcome.landed_sha),
            },
            RollingQueueEntryState::Superseded
                if outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.superseded_by.is_some()) =>
            {
                Self::Superseded {
                    successor: outcome
                        .and_then(|outcome| outcome.superseded_by)
                        .unwrap_or_default(),
                }
            }
            state @ (RollingQueueEntryState::Refused
            | RollingQueueEntryState::Failed
            | RollingQueueEntryState::Superseded) => Self::Refused {
                state: state.as_str().to_owned(),
                reason: outcome
                    .and_then(|outcome| {
                        let detail = outcome.detail.as_deref().map(crate::rolling_queue::tail);
                        match (outcome.refusal, detail) {
                            (Some(code), Some(detail)) => Some(format!("{code}: {detail}")),
                            (code, detail) => code.or(detail),
                        }
                    })
                    .unwrap_or_else(|| state.as_str().to_owned()),
            },
        }
    }
}

/// #3036: what a land refused for a merge conflict does about the conflicting
/// entries ahead of it.
enum ConflictWait {
    /// A recorded entry is still queued, admitted or gating: stay `waiting`.
    Hold,
    /// Settle the refusal now; carries how each waited-for entry ended.
    Settle(Option<String>),
}

/// Typed output of a settled land attempt. `issue_close` is present only for a
/// published land whose step has `close_issue` (#2260).
fn land_output(
    entry: Uuid,
    source_commit: &str,
    state: &str,
    landed_sha: Option<&str>,
    reason: Option<&str>,
    issue_close: Option<&IssueClose>,
) -> serde_json::Value {
    let mut fields = serde_json::Map::new();
    fields.insert("entry_id".into(), entry.to_string().into());
    fields.insert("source_commit".into(), source_commit.into());
    fields.insert("state".into(), state.into());
    if let Some(sha) = landed_sha {
        fields.insert("landed_sha".into(), sha.into());
    }
    if let Some(reason) = reason {
        fields.insert("reason".into(), reason.into());
    }
    if let Some(close) = issue_close {
        fields.insert("issue_close".into(), close.to_json());
    }
    serde_json::to_value(node_data(&serde_json::Value::Object(fields)))
        .unwrap_or(serde_json::Value::Null)
}

/// The accepted review attempt a land attempt forked from: the latest
/// succeeded review of `accepted` whose verdict is `accepted` for exactly the
/// land attempt's base commit.
fn accepted_review<'a>(
    attempts: &'a [AttemptRow],
    accepted: &str,
    commit: &str,
) -> Option<&'a AttemptRow> {
    attempts
        .iter()
        .filter(|other| {
            other.node_kind == "review"
                && other.node_id == accepted
                && other.status == AttemptStatus::Succeeded
                && other.result_commit.as_deref() == Some(commit)
                && other.review_assignment_id.is_some()
                && other
                    .output
                    .as_ref()
                    .and_then(|output| output.pointer("/fields/verdict"))
                    .and_then(serde_json::Value::as_str)
                    == Some("accepted")
        })
        .max_by_key(|other| (other.iteration, other.attempt_no))
}

/// The verified session attempt a land attempt forked from: the latest
/// succeeded attempt of `accepted` whose result commit and recorded
/// `verification.head` both equal the land attempt's base commit (#2249).
/// Returns it with the verifying job's id.
fn accepted_verification<'a>(
    execution: &ExecutionRow,
    attempts: &'a [AttemptRow],
    accepted: &str,
    commit: &str,
) -> Option<(&'a AttemptRow, Uuid)> {
    attempts
        .iter()
        .filter(|other| {
            other.node_kind == "session"
                && other.node_id == accepted
                && other.status == AttemptStatus::Succeeded
                && other.result_commit.as_deref() == Some(commit)
        })
        .filter_map(|other| {
            // The session output carries the whole last message, so it may
            // live in Git: read it through the digest-verified path.
            let data = node_output(&execution.repo_root, other).ok()?;
            let Some(rsi_graph::data::Value::Map(verification)) = data.get("verification") else {
                return None;
            };
            let field = |key: &str| match verification.get(key) {
                Some(rsi_graph::data::Value::String(text)) => Some(text.as_str()),
                _ => None,
            };
            (field("head") == Some(commit))
                .then(|| Uuid::parse_str(field("job_id")?).ok())
                .flatten()
                .map(|job| (other, job))
        })
        .max_by_key(|(other, _)| (other.iteration, other.attempt_no))
}

/// Resolve the landing request for `attempt`; a reason string means the
/// recorded acceptance cannot be tied to this execution's attempts.
fn land_request(
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    attempts: &[AttemptRow],
) -> std::result::Result<LandRequest, String> {
    let shape = crate::topology::graph::GraphShape::from_workflow(&execution.definition)
        .map_err(|error| error.to_string())?;
    let Some(rsi_common::types::TopologyStep::Land {
        accepted,
        test_filters,
        ..
    }) = shape.steps().step(&attempt.node_id)
    else {
        return Err(format!("node {} is not a land node", attempt.node_id));
    };
    let (acceptance, author_session_id) = if shape.steps().require_verified(accepted) {
        // #2249: the accepted session's own receipt admits the commit, and
        // the queue entry's source is that session.
        let (session, job_id) =
            accepted_verification(execution, attempts, accepted, &attempt.base_commit).ok_or_else(
                || {
                    format!(
                        "no verified result of node {accepted} for commit {}",
                        attempt.base_commit
                    )
                },
            )?;
        (
            LandAcceptance::Verified {
                node: accepted.clone(),
                session_id: session.session_id,
                job_id,
                head: attempt.base_commit.clone(),
            },
            session.session_id,
        )
    } else {
        let review =
            accepted_review(attempts, accepted, &attempt.base_commit).ok_or_else(|| {
                format!(
                    "no accepted review of node {accepted} for commit {}",
                    attempt.base_commit
                )
            })?;
        let assignment = review
            .review_assignment_id
            .ok_or_else(|| "the accepted review recorded no assignment".to_owned())?;
        // The queue entry's source is the author of the first reviewed commit,
        // like the review's own ledger Work.
        let first = attempts
            .iter()
            .filter(|other| other.node_kind == "review" && other.node_id == *accepted)
            .min_by_key(|other| (other.iteration, other.attempt_no))
            .map_or(attempt.base_commit.as_str(), |first| {
                first.base_commit.as_str()
            });
        let author = crate::topology::review::producer(attempts, first)
            .ok_or_else(|| format!("no node of this execution produced reviewed commit {first}"))?;
        let acceptance = if let Some(base) = review
            .output
            .as_ref()
            .and_then(|o| o.pointer("/fields/rolling_merge_base"))
            .and_then(|v| v.as_str())
        {
            LandAcceptance::RollingMerge {
                node: accepted.clone(),
                assignment_id: assignment,
                reviewed_commit: base.to_owned(),
            }
        } else {
            LandAcceptance::Review {
                node: accepted.clone(),
                assignment_id: assignment,
            }
        };
        (acceptance, author.session_id)
    };
    // Re-derived from the stored graph and recorded input on every attempt, so
    // an edited or invalid policy blocks before an entry exists.
    let review_coverage = crate::topology::review_policy::enforcement(
        execution.topology_id,
        &execution.definition,
        execution.input.as_ref(),
        &attempt.node_id,
    )?;
    Ok(LandRequest {
        execution_id: execution.id,
        attempt_id: attempt.id,
        dedup_key: attempt.dedup_key.clone(),
        project_id: execution.project_id,
        epic_id: execution.epic_id,
        repo_root: execution.repo_root.clone(),
        source_commit: attempt.base_commit.clone(),
        acceptance,
        author_session_id,
        test_filters: test_filters.clone(),
        review_coverage,
    })
}

impl<E: NodeEffects> Executor<E> {
    pub(crate) async fn drive_land(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        match attempt.status {
            AttemptStatus::Reserved | AttemptStatus::Launching => {
                self.enqueue_land_for(execution, attempt).await
            }
            AttemptStatus::Waiting => self.observe_land(execution, attempt).await,
            _ => Ok(false),
        }
    }

    async fn record_waiting(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        entry: Uuid,
    ) -> Result<bool> {
        let update = {
            let store = self.store.lock().await;
            rows::mark_land_waiting(&store, execution.id, attempt, entry, self.boot_id)?
        };
        self.publish([update]);
        Ok(true)
    }

    async fn enqueue_land_for(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let attempts = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
        };
        let request = match land_request(execution, attempt, &attempts) {
            Ok(request) => request,
            Err(reason) => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    failed(failure::LAND_ADMISSION_LOST, &reason),
                )
                .await?;
                return Ok(true);
            }
        };
        // A crash between the enqueue and its record leaves an entry no
        // attempt row names: adopt it (even while cancelling) rather than
        // enqueue or abandon it.
        if let Some(entry) = self.effects.find_land_entry(&request).await {
            return self.record_waiting(execution, attempt, entry).await;
        }
        if execution.status == ExecutionStatus::Cancelling {
            // Never enqueue into a cancelling execution.
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
        match self.effects.enqueue_land(request).await {
            Ok(LandEnqueue::Queued(entry)) => self.record_waiting(execution, attempt, entry).await,
            Ok(LandEnqueue::AdmissionLost(reason)) => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    failed(failure::LAND_ADMISSION_LOST, &reason),
                )
                .await?;
                Ok(true)
            }
            Ok(LandEnqueue::Cancelled) => {
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
                Ok(true)
            }
            Ok(LandEnqueue::RequiresReview(reason)) => {
                // #2688: a graph with a `review_required` edge out of this
                // land node carries its own pre-land review; the land settles
                // as a routed failure and that edge takes the exact head to
                // the review node. Any other graph still blocks.
                let routed =
                    crate::topology::graph::GraphShape::from_workflow(&execution.definition)
                        .is_ok_and(|shape| shape.steps().routes_review_required(&attempt.node_id));
                let status = if routed {
                    AttemptStatus::Failed
                } else {
                    AttemptStatus::Blocked
                };
                self.settle(
                    execution,
                    attempt,
                    status,
                    failed(failure::LAND_REQUIRES_REVIEW, &reason),
                )
                .await?;
                Ok(true)
            }
            Ok(LandEnqueue::QueueDisabled) => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    failed(
                        failure::QUEUE_DISABLED,
                        "the operator disabled the rolling merge queue; nothing was enqueued",
                    ),
                )
                .await?;
                Ok(true)
            }
            Ok(LandEnqueue::Refused(code)) => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Failed,
                    failed(failure::LAND_REFUSED, &code),
                )
                .await?;
                Ok(true)
            }
            // A transient store or process error is not a refusal: the
            // attempt stays reserved and the next tick asks again.
            Err(error) => {
                tracing::warn!(%error, attempt = %attempt.id, "topology land enqueue deferred");
                Ok(false)
            }
        }
    }

    async fn observe_land(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        let Some(entry) = attempt.land_entry_id else {
            self.settle(
                execution,
                attempt,
                AttemptStatus::Failed,
                failed(failure::LAND_REFUSED, "waiting land has no queue entry"),
            )
            .await?;
            return Ok(true);
        };
        let mut status = self.effects.land_status(entry).await;
        // #3358: an entry replaced in place by a descendant source kept its
        // queue position and wake; follow the successor and settle on its
        // outcome. A cancel without a successor stays a refusal below.
        if let LandStatus::Superseded { successor } = status {
            let update = {
                let store = self.store.lock().await;
                rows::follow_land_entry(&store, execution.id, attempt, entry, successor)?
            };
            if let Some(update) = update {
                self.publish([update]);
            }
            return Ok(true);
        }
        let waited = match self
            .conflict_wait(execution, attempt, entry, &status)
            .await?
        {
            ConflictWait::Hold => return Ok(false),
            ConflictWait::Settle(waited) => waited,
        };
        // #2599: a refused entry whose accepted source is already an ancestor
        // of a freshly fetched `origin/rolling` did land (a false ancestry
        // verdict, a landing by another entry): settle it as landed instead of
        // routing to a repair that has nothing to commit.
        if matches!(status, LandStatus::Refused { .. })
            && let Some(tip) = self
                .effects
                .landed_source_tip(execution.repo_root.clone(), attempt.base_commit.clone())
                .await
        {
            status = LandStatus::Published {
                landed_sha: Some(tip),
            };
        }
        match status {
            // Followed above, before the conflict wait and the ancestry check.
            LandStatus::Pending | LandStatus::Superseded { .. } => Ok(false),
            LandStatus::Published { landed_sha } => {
                // #2260: close the input Issue before settling. A transient
                // error is not an outcome: the entry stays published and the
                // next tick re-runs the idempotent close.
                let issue_close = match self
                    .close_land_issue(execution, attempt, entry, landed_sha.as_deref())
                    .await
                {
                    Ok(close) => close,
                    Err(error) => {
                        tracing::warn!(%error, attempt = %attempt.id, "topology land issue close deferred");
                        return Ok(false);
                    }
                };
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Succeeded,
                    Settlement {
                        // The accepted commit stays the node's result so
                        // downstream custody forks something this repository
                        // holds; the published tip is in the typed output.
                        result_commit: Some(attempt.base_commit.clone()),
                        output: Some(land_output(
                            entry,
                            &attempt.base_commit,
                            "published",
                            landed_sha.as_deref(),
                            None,
                            issue_close.as_ref(),
                        )),
                        ..Settlement::default()
                    },
                )
                .await?;
                // #2607: the published land integrates the review rows whose
                // accepted source it carried. A failure is not an outcome: the
                // settlement pass of the finished execution repeats it.
                if let Err(error) = self
                    .settle_review_work(execution.id, execution.repo_root.clone(), false)
                    .await
                {
                    tracing::warn!(%error, execution = %execution.id, "topology review work integration deferred");
                }
                Ok(true)
            }
            LandStatus::Refused { state, reason } => {
                let error = match waited {
                    Some(waited) => format!("merge queue entry {state}: {reason}; {waited}"),
                    None => format!("merge queue entry {state}: {reason}"),
                };
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Failed,
                    Settlement {
                        failure_class: Some(failure::LAND_REFUSED),
                        error: Some(error),
                        output: Some(land_output(
                            entry,
                            &attempt.base_commit,
                            &state,
                            None,
                            Some(&reason),
                            None,
                        )),
                        ..Settlement::default()
                    },
                )
                .await?;
                Ok(true)
            }
        }
    }

    /// #3036: a land the queue refused `queue_batch_merge_conflict` while a
    /// conflicting entry ahead of it is still queued, admitted or gating waits
    /// for that entry to publish or be refused before it settles, so the
    /// repair it routes to merges a rolling that already holds (or will never
    /// hold) that entry instead of racing it. The entries are chosen once
    /// (`land_conflict_wait` event) and each later tick only reads their
    /// states. Any other refusal, a refusal with nothing ahead, and a
    /// cancelling execution settle at once.
    async fn conflict_wait(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        entry: Uuid,
        status: &LandStatus,
    ) -> Result<ConflictWait> {
        let LandStatus::Refused { reason, .. } = status else {
            return Ok(ConflictWait::Settle(None));
        };
        if !reason.starts_with(rsi_common::rolling_queue::QUEUE_BATCH_MERGE_CONFLICT)
            || execution.status == ExecutionStatus::Cancelling
        {
            return Ok(ConflictWait::Settle(None));
        }
        let recorded = {
            let store = self.store.lock().await;
            rows::land_conflict_wait(&store, attempt.id)?
        };
        let waiting_for = match recorded {
            Some(entries) => entries,
            None => self.record_conflict_wait(execution, attempt, entry).await?,
        };
        if waiting_for.is_empty() {
            return Ok(ConflictWait::Settle(None));
        }
        let mut ended = Vec::with_capacity(waiting_for.len());
        for blocker in &waiting_for {
            let state = match self.effects.land_status(*blocker).await {
                LandStatus::Pending => return Ok(ConflictWait::Hold),
                LandStatus::Published { .. } => "published".to_owned(),
                LandStatus::Superseded { .. } => "superseded".to_owned(),
                LandStatus::Refused { state, .. } => state,
            };
            ended.push(format!("{blocker} {state}"));
        }
        Ok(ConflictWait::Settle(Some(format!(
            "waited for the conflicting queue entries ahead to settle: {}",
            ended.join(", ")
        ))))
    }

    /// #3036: choose the conflicting live entries ahead of refused `entry` and
    /// record them (none recorded when there are none: the refusal settles).
    async fn record_conflict_wait(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        entry: Uuid,
    ) -> Result<Vec<Uuid>> {
        let blockers = self
            .effects
            .land_conflict_blockers(execution.repo_root.clone(), execution.project_id, entry)
            .await;
        if blockers.is_empty() {
            return Ok(blockers);
        }
        let update = {
            let store = self.store.lock().await;
            rows::note_land_conflict_wait(&store, execution.id, attempt, entry, &blockers)?
        };
        if let Some(update) = update {
            self.publish([update]);
        }
        Ok(blockers)
    }

    /// #2260: close the execution's input Issue for a published land whose step
    /// has `close_issue`, and note it on the execution. `None`: the step does
    /// not close an Issue. A closed Issue gets one `topology_landed` event; a
    /// refused owner gets one `issue_close_refused` (the Issue stays open for
    /// the owner). Both are written at most once per attempt, so a crash
    /// between the Issue write and the settlement leaves one note, not two.
    ///
    /// # Errors
    /// A transient store error; the caller asks again on its next tick.
    async fn close_land_issue(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        entry: Uuid,
        landed_sha: Option<&str>,
    ) -> Result<Option<IssueClose>> {
        let (outcome, issue) = match IssueCloseRequest::for_land(execution, attempt) {
            None => return Ok(None),
            Some(Err(refused)) => (refused, None),
            Some(Ok(request)) => {
                let store = self.store.lock().await;
                (
                    close_input_issue(&store, &request)?,
                    Some(request.display_number),
                )
            }
        };
        let kind = match &outcome {
            IssueClose::Closed => "topology_landed",
            IssueClose::Refused(_) => "issue_close_refused",
            IssueClose::AlreadyClosed | IssueClose::Skipped(_) | IssueClose::Held(_) => {
                return Ok(Some(outcome));
            }
        };
        let update = {
            let store = self.store.lock().await;
            rows::note_land_issue(
                &store,
                execution.id,
                attempt,
                execution.requested_by_session_id,
                kind,
                serde_json::json!({
                    "execution_id": execution.id,
                    "node_id": attempt.node_id,
                    "landed_sha": landed_sha,
                    "source_commit": attempt.base_commit,
                    "entry_id": entry,
                    "issue": issue,
                    "code": outcome.code(),
                }),
            )?
        };
        if let Some(update) = update {
            self.publish([update]);
        }
        Ok(Some(outcome))
    }
}
