//! Typed resolution of preserved work (#634, plan §3.4).
//!
//! Operator-only in T2 (`ResolveTopologyAttempt`); T4 adds the scoped agent
//! verb over the same function. Every action is CAS-fenced on the execution
//! `row_version`, idempotent per key, and audited in `topology_events`.

use rsi_common::rpc::{
    ResolveTopologyAttemptParams, ResolveTopologyAttemptResponse, TopologyAttemptAction,
    TopologyAttemptInspection, TopologyAttemptSummary,
};
use serde_json::json;

use crate::error::{DaemonError, Result};
use crate::topology::custody::{self, node_pin_ref};
use crate::topology::executor::{Executor, NodeEffects};
use crate::topology::graph::MAX_ATTEMPTS_PER_NODE;
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, NewAttempt, ResolutionGate,
    ResolutionWrite, failure,
};

const IDEMPOTENCY_KEY_MAX: usize = 128;

/// Redacted `code`/`next_action` envelope (plan §5.1 error shape).
pub(crate) fn resolution_error(
    code: &str,
    next_action: &str,
    expected_row_version: Option<i64>,
    actual_row_version: Option<i64>,
) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: code.to_owned(),
        data: json!({
            "code": code,
            "next_action": next_action,
            "expected_row_version": expected_row_version,
            "actual_row_version": actual_row_version,
        }),
    }
}

fn invalid_params(next_action: &str) -> DaemonError {
    resolution_error("invalid_params", next_action, None, None)
}

fn is_full_oid(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate(params: &ResolveTopologyAttemptParams) -> Result<()> {
    if params.idempotency_key.trim().is_empty()
        || params.idempotency_key.len() > IDEMPOTENCY_KEY_MAX
    {
        return Err(invalid_params(
            "supply a non-empty idempotency_key of at most 128 bytes",
        ));
    }
    match (params.action, params.confirm_preserved_commit.as_deref()) {
        (TopologyAttemptAction::Discard, Some(commit)) if is_full_oid(commit) => Ok(()),
        (TopologyAttemptAction::Discard, _) => Err(invalid_params(
            "discard requires confirm_preserved_commit: the full 40-hex preserved commit",
        )),
        (_, Some(_)) => Err(invalid_params(
            "confirm_preserved_commit is accepted only with action=discard",
        )),
        (_, None) => Ok(()),
    }
}

fn summary(attempt: &AttemptRow) -> TopologyAttemptSummary {
    TopologyAttemptSummary {
        attempt_id: attempt.id,
        node_id: attempt.node_id.clone(),
        iteration: attempt.iteration,
        attempt_no: attempt.attempt_no,
        status: attempt.status.as_str().to_owned(),
        failure_class: attempt.failure_class.clone(),
        resolution: attempt.resolution.clone(),
        base_commit: attempt.base_commit.clone(),
        result_commit: attempt.result_commit.clone(),
        preserved_ref: attempt.preserved_ref.clone(),
        preserved_commit: attempt.preserved_commit.clone(),
    }
}

fn precondition(next_action: &str) -> DaemonError {
    resolution_error("precondition_failed", next_action, None, None)
}

impl<E: NodeEffects> Executor<E> {
    async fn load_pair(
        &self,
        params: &ResolveTopologyAttemptParams,
    ) -> Result<(ExecutionRow, AttemptRow)> {
        let store = self.store.lock().await;
        let not_found = || {
            resolution_error(
                "not_found",
                "refresh the execution and name one of its attempts",
                None,
                None,
            )
        };
        let (execution_id, attempt) =
            rows::load_attempt(&store, params.attempt_id)?.ok_or_else(not_found)?;
        if execution_id != params.execution_id {
            return Err(not_found());
        }
        let execution = rows::load_execution(&store, execution_id)?.ok_or_else(not_found)?;
        Ok((execution, attempt))
    }

    async fn respond(
        &self,
        params: &ResolveTopologyAttemptParams,
        report: Option<TopologyAttemptInspection>,
        deduplicated: bool,
    ) -> Result<ResolveTopologyAttemptResponse> {
        let (execution, attempt) = self.load_pair(params).await?;
        Ok(ResolveTopologyAttemptResponse {
            attempt: summary(&attempt),
            execution_status: execution.status.wire(),
            row_version: execution.row_version,
            report,
            deduplicated,
        })
    }

    /// Apply one operator resolution. The caller wakes the execution driver.
    pub(crate) async fn resolve_attempt(
        &self,
        params: &ResolveTopologyAttemptParams,
    ) -> Result<ResolveTopologyAttemptResponse> {
        validate(params)?;
        let (execution, attempt) = self.load_pair(params).await?;
        let fingerprint = rows::resolution_fingerprint(
            attempt.id,
            params.action.as_str(),
            params.expected_row_version,
            params.confirm_preserved_commit.as_deref(),
        );
        let gate = rows::resolution_gate(
            &*self.store.lock().await,
            execution.id,
            &params.idempotency_key,
            &fingerprint,
        )?;
        if matches!(gate, ResolutionGate::Replay) {
            // A replayed discard also finishes any interrupted phase 2.
            if params.action == TopologyAttemptAction::Discard {
                self.discard_effects(&execution, &attempt).await?;
            }
            return self.respond(params, None, true).await;
        }
        if execution.row_version != params.expected_row_version {
            return Err(resolution_error(
                "stale_row_version",
                "refresh the execution and retry with its current row_version",
                Some(params.expected_row_version),
                Some(execution.row_version),
            ));
        }
        self.effects.before_resolution_record(execution.id).await;
        if params.action == TopologyAttemptAction::Inspect {
            let report = Self::inspect(&attempt);
            let recorded = rows::record_inspection(
                &*self.store.lock().await,
                execution.id,
                &attempt,
                &params.idempotency_key,
                &fingerprint,
            )?;
            return match recorded {
                rows::Recorded::Fresh(updates) => {
                    for update in updates {
                        self.effects.publish(update);
                    }
                    self.respond(params, Some(report), false).await
                }
                rows::Recorded::Replay => self.respond(params, None, true).await,
            };
        }
        if attempt.status != AttemptStatus::Blocked
            || attempt.failure_class.as_deref() != Some(failure::PRESERVED_WORK)
        {
            return Err(precondition(
                "only a blocked preserved-work attempt can be accepted, retried or discarded",
            ));
        }
        let mut write = match params.action {
            TopologyAttemptAction::Accept => Self::accept_write(&execution, &attempt, params)?,
            TopologyAttemptAction::Retry => self.retry_write(&execution, &attempt, params).await?,
            TopologyAttemptAction::Discard => discard_write(&execution, &attempt, params)?,
            TopologyAttemptAction::Inspect => unreachable!("inspect returned above"),
        };
        write.fingerprint = &fingerprint;
        let recorded = rows::write_resolution(&*self.store.lock().await, write)?;
        let deduplicated = match recorded {
            rows::Recorded::Fresh(updates) => {
                for update in updates {
                    self.effects.publish(update);
                }
                false
            }
            // A concurrent identical request recorded first.
            rows::Recorded::Replay => true,
        };
        if params.action == TopologyAttemptAction::Discard {
            // Phase 2: destroy bytes, then resume. A crash or effect error
            // leaves the discard pending for the replay or the recovery pass.
            let (execution, attempt) = self.load_pair(params).await?;
            self.discard_effects(&execution, &attempt).await?;
        }
        self.respond(params, None, deduplicated).await
    }

    fn inspect(attempt: &AttemptRow) -> TopologyAttemptInspection {
        let observed = attempt
            .sandbox_root
            .as_deref()
            .filter(|root| root.exists())
            .and_then(|root| custody::inspect_sandbox(root, &attempt.base_commit).ok());
        TopologyAttemptInspection {
            head: observed.as_ref().map(|report| report.head.clone()),
            base_commit: attempt.base_commit.clone(),
            preserved_commit: attempt.preserved_commit.clone(),
            dirty_paths: observed
                .as_ref()
                .map(|report| report.dirty_paths.clone())
                .unwrap_or_default(),
            diffstat: observed.map(|report| report.diffstat).unwrap_or_default(),
        }
    }

    /// `accept`: clean tree and HEAD≠base ⇒ `result_commit` = HEAD (pinned).
    fn accept_write<'a>(
        execution: &ExecutionRow,
        attempt: &'a AttemptRow,
        params: &'a ResolveTopologyAttemptParams,
    ) -> Result<ResolutionWrite<'a>> {
        let sandbox = attempt
            .sandbox_root
            .as_deref()
            .filter(|root| root.exists())
            .ok_or_else(|| precondition("the attempt sandbox is no longer available"))?;
        let observed = custody::observe_sandbox(sandbox)?;
        if observed.dirty || observed.head == attempt.base_commit {
            return Err(precondition(
                "accept requires a clean tree whose HEAD differs from the attempt base",
            ));
        }
        let pin = node_pin_ref(execution.id, &attempt.node_id, attempt.iteration);
        custody::pin_commit(sandbox, &pin, &observed.head)?;
        Ok(ResolutionWrite {
            execution_id: execution.id,
            attempt,
            expected_row_version: params.expected_row_version,
            idempotency_key: &params.idempotency_key,
            action: params.action.as_str(),
            attempt_status: AttemptStatus::Succeeded,
            resolution: Some("accepted"),
            failure_class: None,
            detail: json!({ "result_commit": observed.head }),
            result_commit: Some(observed.head),
            pin_ref: Some(pin),
            retry: None,
            event_kind: "preserved_work_accepted",
            fingerprint: "",
            resume: true,
        })
    }

    /// `retry`: only from a verified preservation point, as a new `attempt_no`
    /// forked from `preserved_commit`. The old sandbox and ref are kept.
    async fn retry_write<'a>(
        &self,
        execution: &ExecutionRow,
        attempt: &'a AttemptRow,
        params: &'a ResolveTopologyAttemptParams,
    ) -> Result<ResolutionWrite<'a>> {
        let attempts = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
        };
        let charged = attempts
            .iter()
            .filter(|row| row.node_id == attempt.node_id && row.iteration == attempt.iteration)
            .filter(|row| row.failure_class.as_deref() != Some(failure::LOST_BEFORE_SESSION))
            .count();
        if charged >= MAX_ATTEMPTS_PER_NODE as usize
            || attempts
                .iter()
                .filter(|row| !failure::is_infrastructure(row.failure_class.as_deref()))
                .count()
                >= crate::topology::graph::GraphShape::from_workflow(&execution.definition)?
                    .attempt_cap(execution.max_node_attempts) as usize
        {
            return Err(resolution_error(
                "attempts_exhausted",
                "accept or discard the preserved work instead",
                None,
                None,
            ));
        }
        let verified = match (&attempt.preserved_ref, &attempt.preserved_commit) {
            (Some(name), Some(commit)) => {
                custody::ref_points_at(&execution.repo_root, name, commit)
            }
            _ => false,
        };
        let Some(preserved_commit) = attempt.preserved_commit.clone().filter(|_| verified) else {
            return Err(resolution_error(
                "preservation_point_unverified",
                "the preservation ref is missing or moved; inspect before retrying",
                None,
                None,
            ));
        };
        let (node_kind, catalog_op, effect_class) = crate::topology::executor::attempt_kind(
            &crate::topology::steps::WorkflowSteps::from_workflow(&execution.definition)
                .unwrap_or_default(),
            &attempt.node_id,
        );
        Ok(ResolutionWrite {
            execution_id: execution.id,
            attempt,
            expected_row_version: params.expected_row_version,
            idempotency_key: &params.idempotency_key,
            action: params.action.as_str(),
            attempt_status: AttemptStatus::Failed,
            resolution: Some("retried"),
            failure_class: None,
            result_commit: None,
            pin_ref: None,
            detail: json!({ "base_commit": preserved_commit, "attempt_no": attempt.attempt_no + 1 }),
            retry: Some(NewAttempt {
                node_id: attempt.node_id.clone(),
                iteration: attempt.iteration,
                attempt_no: attempt.attempt_no + 1,
                base_commit: preserved_commit,
                query: attempt.query().to_owned(),
                node_kind,
                catalog_op,
                effect_class,
            }),
            event_kind: "preserved_work_retried",
            fingerprint: "",
            resume: true,
        })
    }

    /// Phase 2 of a discard, recorded before any byte is destroyed so a
    /// crash never leaves an unrecorded deletion. Every step is idempotent:
    /// a ref already gone and a session already released are success; the
    /// execution resumes only after both effects succeed.
    async fn discard_effects(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<()> {
        if attempt.resolution.as_deref() != Some("discarded") || attempt.preserved_ref.is_none() {
            return Ok(());
        }
        if let (Some(name), Some(commit)) = (&attempt.preserved_ref, &attempt.preserved_commit)
            && custody::ref_points_at(&execution.repo_root, name, commit)
        {
            custody::delete_ref(&execution.repo_root, name, commit)?;
        }
        self.effects.release_sandbox(attempt.session_id).await?;
        let updates = rows::finish_discard(&*self.store.lock().await, execution.id, attempt)?;
        for update in updates {
            self.effects.publish(update);
        }
        Ok(())
    }

    /// Recovery: finish every discard whose phase 2 did not complete.
    pub(crate) async fn complete_pending_discards(&self, limit: usize) -> Result<usize> {
        let pending = rows::pending_discards(&*self.store.lock().await, limit)?;
        let mut completed = 0;
        for (execution_id, attempt_id) in pending {
            let pair = {
                let store = self.store.lock().await;
                (
                    rows::load_execution(&store, execution_id)?,
                    rows::load_attempt(&store, attempt_id)?,
                )
            };
            if let (Some(execution), Some((_, attempt))) = pair {
                match self.discard_effects(&execution, &attempt).await {
                    Ok(()) => completed += 1,
                    Err(error) => {
                        tracing::warn!(%attempt_id, %error, "pending topology discard deferred");
                    }
                }
            }
        }
        Ok(completed)
    }
}

fn discard_write<'a>(
    execution: &ExecutionRow,
    attempt: &'a AttemptRow,
    params: &'a ResolveTopologyAttemptParams,
) -> Result<ResolutionWrite<'a>> {
    let confirm = params
        .confirm_preserved_commit
        .as_deref()
        .map(str::to_ascii_lowercase);
    if attempt.preserved_commit.is_none() || confirm != attempt.preserved_commit {
        return Err(resolution_error(
            "preserved_commit_mismatch",
            "inspect the attempt and confirm its exact preserved_commit",
            None,
            None,
        ));
    }
    Ok(ResolutionWrite {
        execution_id: execution.id,
        attempt,
        expected_row_version: params.expected_row_version,
        idempotency_key: &params.idempotency_key,
        action: params.action.as_str(),
        attempt_status: AttemptStatus::Failed,
        resolution: Some("discarded"),
        failure_class: Some(failure::DISCARDED),
        result_commit: None,
        pin_ref: None,
        detail: json!({
            "actor_kind": "operator",
            "preserved_commit": attempt.preserved_commit,
            "preserved_ref": attempt.preserved_ref,
        }),
        retry: None,
        event_kind: "preserved_work_discarded",
        fingerprint: "",
        resume: false,
    })
}
