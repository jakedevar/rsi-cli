//! Durable driving of land nodes (#1641 S2).
//!
//! A land attempt publishes the commit an accepted review admitted by
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
//! - the execution is cancelled before the entry exists (including while the
//!   Git probe ran): `cancelled`, nothing enqueued; an entry created before
//!   the cancellation is still adopted and mirrored;
//! - the merge queue is disabled: `blocked(queue_disabled)`, nothing enqueued;
//! - the queue refuses the source (invalid, duplicate, a filter selecting no
//!   test): `failed(land_refused)`;
//! - the entry ends `published`: `succeeded` with the landed tip in its typed
//!   output; ends `refused`, `failed` or `superseded`: `failed(land_refused)`,
//!   never retried (a second landing is a decision, not a retry).
//!
//! A queued landing is not bounded by the node wall time: the queue has its
//! own gate budget and always settles an entry, and failing the node while the
//! entry might still publish would make the record lie.

use std::path::PathBuf;

use uuid::Uuid;

use crate::error::Result;
use crate::topology::executor::{Executor, NodeEffects, failed, node_data};
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, Settlement, failure,
};

/// One landing request: everything the queue admission needs, taken from the
/// execution and the accepted review attempt.
#[derive(Clone, Debug)]
pub(crate) struct LandRequest {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) node_id: String,
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
    /// The review node and the assignment that accepted `source_commit`.
    pub(crate) review_node: String,
    pub(crate) review_assignment_id: Uuid,
    /// The reviewed author session: the queue entry's source identity.
    pub(crate) author_session_id: Uuid,
    pub(crate) test_filters: Vec<String>,
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
    /// Refused, failed or superseded (or the entry vanished).
    Refused {
        state: String,
        reason: String,
    },
}

/// Typed output of a settled land attempt.
fn land_output(
    entry: Uuid,
    source_commit: &str,
    state: &str,
    landed_sha: Option<&str>,
    reason: Option<&str>,
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
    }) = shape.steps().step(&attempt.node_id)
    else {
        return Err(format!("node {} is not a land node", attempt.node_id));
    };
    let review = accepted_review(attempts, accepted, &attempt.base_commit).ok_or_else(|| {
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
    Ok(LandRequest {
        execution_id: execution.id,
        attempt_id: attempt.id,
        node_id: attempt.node_id.clone(),
        dedup_key: attempt.dedup_key.clone(),
        project_id: execution.project_id,
        epic_id: execution.epic_id,
        repo_root: execution.repo_root.clone(),
        source_commit: attempt.base_commit.clone(),
        review_node: accepted.clone(),
        review_assignment_id: assignment,
        author_session_id: author.session_id,
        test_filters: test_filters.clone(),
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
        match self.effects.land_status(entry).await {
            LandStatus::Pending => Ok(false),
            LandStatus::Published { landed_sha } => {
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
                        )),
                        ..Settlement::default()
                    },
                )
                .await?;
                Ok(true)
            }
            LandStatus::Refused { state, reason } => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Failed,
                    Settlement {
                        failure_class: Some(failure::LAND_REFUSED),
                        error: Some(format!("merge queue entry {state}: {reason}")),
                        output: Some(land_output(
                            entry,
                            &attempt.base_commit,
                            &state,
                            None,
                            Some(&reason),
                        )),
                        ..Settlement::default()
                    },
                )
                .await?;
                Ok(true)
            }
        }
    }
}
