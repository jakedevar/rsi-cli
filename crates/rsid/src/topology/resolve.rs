//! Typed resolution of preserved work (#634, plan §3.4).
//!
//! The operator RPC `ResolveTopologyAttempt` (T2) and the scoped agent verb
//! `AgentTopologyResolveAttempt` (T4, #633) run the same function; only the
//! recorded actor differs. Every action is CAS-fenced on the execution
//! `row_version`, idempotent per key, and audited in `topology_events`.

use rsi_common::rpc::{
    ResolveTopologyAttemptParams, ResolveTopologyAttemptResponse, TopologyAttemptAction,
    TopologyAttemptInspection, TopologyAttemptSummary,
};
use serde_json::json;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::topology::custody::{self, node_pin_ref};
use crate::topology::executor::{Executor, NodeEffects};
use crate::topology::graph::MAX_ATTEMPTS_PER_NODE;
use crate::topology::review::{ExtraRound, OncallAcceptance};
use crate::topology::store::{
    self as rows, Actor, AttemptRow, AttemptStatus, ExecutionRow, NewAttempt, ResolutionGate,
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

pub(crate) fn summary(attempt: &AttemptRow) -> TopologyAttemptSummary {
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

/// What a blocked review attempt's `accept` records (#2280): the assignment the
/// owning manager's ruling admits, and the typed review output the edges read.
struct RuledReview {
    assignment: Uuid,
    output: serde_json::Value,
}

/// A blocked review attempt a retry or ruling can resolve: the review gave no
/// usable verdict, or ran out of rounds with nobody to rule.
fn review_resolvable(class: Option<&str>) -> bool {
    matches!(
        class,
        Some(failure::REVIEW_UNSETTLED | failure::REVIEW_ROUNDS_EXHAUSTED)
    )
}

/// What can be resolved, and what to do with every other blocked attempt
/// (#2639): the refusals below name the attempt's real state and this list, so
/// a caller never has to guess which blocked executions `resolve_attempt` can
/// move.
const RESOLVABLE_STATES: &str = "Resolvable: a blocked implement or repair attempt with \
    failure_class preserved_work (inspect, accept, retry, discard), a review blocked \
    review_unsettled or review_rounds_exhausted (retry, accept with a ruling), and a land \
    blocked land_admission_lost (retry only). For any other blocked cause (for example \
    policy_refused) read the execution's blocked_reason with AgentTopologyGetExecution, \
    clear that cause at its source, or end the execution with AgentTopologyInterrupt";

/// A refusal that names the attempt's node kind, status and failure class
/// beside why `action` cannot resolve it and what can be resolved.
fn not_resolvable(attempt: &AttemptRow, why: &str) -> DaemonError {
    precondition(&format!(
        "{why} (this {} attempt is {} with failure_class {}). {RESOLVABLE_STATES}",
        attempt.node_kind,
        attempt.status.as_str(),
        attempt.failure_class.as_deref().unwrap_or("none"),
    ))
}

/// Whether `action` may resolve `attempt` (plan §3.4; #2280 for review nodes).
fn require_resolvable(attempt: &AttemptRow, action: TopologyAttemptAction) -> Result<()> {
    if attempt.status != AttemptStatus::Blocked {
        return Err(not_resolvable(
            attempt,
            "only a blocked attempt can be accepted, retried or discarded; a settled attempt \
             is finished and the executor continues the execution on its own",
        ));
    }
    if attempt.node_kind == "review" {
        if action == TopologyAttemptAction::Discard {
            return Err(precondition(
                "a review attempt holds no preserved work, so there is nothing to discard; \
                 retry the review or accept it with a recorded ruling",
            ));
        }
        if !review_resolvable(attempt.failure_class.as_deref()) {
            return Err(not_resolvable(
                attempt,
                "only a review blocked without a usable verdict or out of rounds can be \
                 accepted or retried",
            ));
        }
        return Ok(());
    }
    if attempt.node_kind == "land" {
        // #2548: a land blocked because its admission was lost (the requester's
        // seat rotated, the review stopped admitting) holds no work: the only
        // recovery is to ask the queue again, which re-checks authority.
        if attempt.failure_class.as_deref() != Some(failure::LAND_ADMISSION_LOST) {
            return Err(not_resolvable(
                attempt,
                "only a land blocked by lost admission (land_admission_lost) can be retried",
            ));
        }
        if action != TopologyAttemptAction::Retry {
            return Err(precondition(
                "a land attempt blocked by lost admission holds no preserved work; retry it \
                 once its authority is restored, or interrupt the execution",
            ));
        }
        return Ok(());
    }
    if attempt.failure_class.as_deref() != Some(failure::PRESERVED_WORK) {
        return Err(not_resolvable(
            attempt,
            "only a blocked preserved-work attempt can be accepted, retried or discarded",
        ));
    }
    Ok(())
}

/// The latest recorded review assignment for exactly the commit the blocked
/// `attempt` reviews, and the attempt that requested it. A retry that was
/// refused before it got an assignment is ruled through the assignment of the
/// earlier attempt it replaced.
fn ruled_assignment<'a>(
    attempts: &'a [AttemptRow],
    attempt: &AttemptRow,
) -> Option<(Uuid, &'a AttemptRow)> {
    attempts
        .iter()
        .filter(|other| {
            other.node_kind == "review"
                && other.node_id == attempt.node_id
                && other.base_commit == attempt.base_commit
        })
        .filter_map(|other| other.review_assignment_id.map(|id| (id, other)))
        .max_by_key(|(_, other)| (other.iteration, other.attempt_no))
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
        self.resolve_attempt_as(params, &|_: &Store| Ok(Actor::OPERATOR))
            .await
    }

    /// Apply one resolution attributed to the actor `authorize` returns
    /// (#633). `authorize` runs under every store guard that decides or
    /// records the resolution — the replay gate, the inspection record and
    /// the accept/retry/discard write — before any effect, so authority
    /// revoked during an await refuses and records nothing. The caller
    /// wakes the execution driver.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "authority is re-checked and the resolution recorded under one guard"
    )]
    pub(crate) async fn resolve_attempt_as(
        &self,
        params: &ResolveTopologyAttemptParams,
        authorize: &(dyn Fn(&Store) -> Result<Actor> + Send + Sync),
    ) -> Result<ResolveTopologyAttemptResponse> {
        validate(params)?;
        let (execution, attempt) = self.load_pair(params).await?;
        let fingerprint = rows::resolution_fingerprint(
            attempt.id,
            params.action.as_str(),
            params.expected_row_version,
            params.confirm_preserved_commit.as_deref(),
        );
        let gate = {
            let store = self.store.lock().await;
            authorize(&store)?;
            rows::resolution_gate(&store, execution.id, &params.idempotency_key, &fingerprint)?
        };
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
            let recorded = {
                let store = self.store.lock().await;
                let actor = authorize(&store)?;
                rows::record_inspection(
                    &store,
                    execution.id,
                    &attempt,
                    &params.idempotency_key,
                    &fingerprint,
                    actor,
                )?
            };
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
        require_resolvable(&attempt, params.action)?;
        // A review's ruling is a review-ledger fact recorded before the
        // attempt settles, so the land node's admission re-check admits it.
        let ruled = if attempt.node_kind == "review" {
            self.prepare_review_resolution(&execution, &attempt, params, authorize)
                .await?
        } else {
            None
        };
        // One guard authorizes, builds (the accept pin is the first effect)
        // and records the resolution.
        let recorded = {
            let store = self.store.lock().await;
            let actor = authorize(&store)?;
            let mut write = match params.action {
                TopologyAttemptAction::Accept => match ruled {
                    Some(ruled) => {
                        Self::review_accept_write(&execution, &attempt, params, actor, ruled)
                    }
                    None => Self::accept_write(&store, &execution, &attempt, params, actor)?,
                },
                TopologyAttemptAction::Retry => {
                    Self::retry_write(&store, &execution, &attempt, params, actor)?
                }
                TopologyAttemptAction::Discard => {
                    discard_write(&execution, &attempt, params, actor)?
                }
                TopologyAttemptAction::Inspect => unreachable!("inspect returned above"),
            };
            write.fingerprint = &fingerprint;
            rows::write_resolution(&store, write)?
        };
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

    /// The async half of resolving a blocked review attempt (#2280), run before
    /// the recording guard. `retry` refuses a doomed attempt (the review
    /// ledger's round policy offers no round); `accept` records the owning
    /// manager's ruling on the review ledger for exactly the reviewed commit.
    /// The ruling is idempotent and admits only that assignment and commit, so
    /// a CAS loss after it leaves the attempt blocked and the fact harmless.
    async fn prepare_review_resolution(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        params: &ResolveTopologyAttemptParams,
        authorize: &(dyn Fn(&Store) -> Result<Actor> + Send + Sync),
    ) -> Result<Option<RuledReview>> {
        match params.action {
            TopologyAttemptAction::Retry => {
                if let ExtraRound::Unavailable(reason) =
                    self.review_retry_round(execution, attempt).await?
                {
                    return Err(resolution_error(
                        "review_round_unavailable",
                        &format!(
                            "no further review round is available ({reason}); accept the review \
                             with a recorded ruling, or interrupt the execution"
                        ),
                        None,
                        None,
                    ));
                }
                Ok(None)
            }
            TopologyAttemptAction::Accept => {
                let attempts = {
                    let store = self.store.lock().await;
                    rows::load_attempts(&store, execution.id)?
                };
                let Some((assignment, requester)) = ruled_assignment(&attempts, attempt) else {
                    return Err(precondition(
                        "the blocked review recorded no assignment to rule on; retry the review",
                    ));
                };
                // Authority is rechecked right before the ledger fact.
                authorize(&*self.store.lock().await)?;
                let (_, reviewer, ..) = Self::review_spec(execution, &attempt.node_id)?;
                self.effects
                    .record_oncall_acceptance(OncallAcceptance {
                        execution_id: execution.id,
                        attempt_id: requester.id,
                        node_id: attempt.node_id.clone(),
                        assignment_id: assignment,
                        commit: attempt.base_commit.clone(),
                        decision_key: format!("resolve:{}", params.idempotency_key),
                    })
                    .await
                    .map_err(|error| {
                        resolution_error(
                            "review_acceptance_refused",
                            &format!("the review ledger refused the ruling: {error}"),
                            None,
                            None,
                        )
                    })?;
                let mut output = crate::topology::review::review_output(
                    attempt.iteration.saturating_add(1),
                    assignment,
                    "accepted",
                    &attempt.base_commit,
                    &reviewer.model,
                    &[],
                );
                if let Some(fields) = output
                    .get_mut("fields")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    fields.insert("ruling".into(), "accepted_by_ruling".into());
                }
                Ok(Some(RuledReview { assignment, output }))
            }
            _ => Ok(None),
        }
    }

    /// `accept` of a blocked review: the attempt succeeds as an accepted
    /// review of its own commit under the recorded ruling, so the land node's
    /// admission re-check finds the same assignment and commit.
    fn review_accept_write<'a>(
        execution: &ExecutionRow,
        attempt: &'a AttemptRow,
        params: &'a ResolveTopologyAttemptParams,
        actor: Actor,
        ruled: RuledReview,
    ) -> ResolutionWrite<'a> {
        ResolutionWrite {
            execution_id: execution.id,
            attempt,
            expected_row_version: params.expected_row_version,
            idempotency_key: &params.idempotency_key,
            action: params.action.as_str(),
            attempt_status: AttemptStatus::Succeeded,
            resolution: Some("accepted"),
            failure_class: None,
            detail: json!({
                "result_commit": attempt.base_commit,
                "assignment_id": ruled.assignment,
                "ruling": "accepted_by_ruling",
            }),
            result_commit: Some(attempt.base_commit.clone()),
            pin_ref: None,
            output: Some(ruled.output),
            review_assignment: Some(ruled.assignment),
            retry: None,
            event_kind: "review_ruling_accepted",
            fingerprint: "",
            resume: true,
            actor,
        }
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

    /// `accept`: a preserved commit ⇒ `result_commit` = that commit (pinned, #2563);
    /// otherwise a clean tree and HEAD≠base ⇒ `result_commit` = HEAD (pinned).
    fn accept_write<'a>(
        store: &Store,
        execution: &ExecutionRow,
        attempt: &'a AttemptRow,
        params: &'a ResolveTopologyAttemptParams,
        actor: Actor,
    ) -> Result<ResolutionWrite<'a>> {
        // #2563: preserved work carries a verified preserved commit, clean by
        // construction (a dirty tree was committed onto the preservation ref).
        // Accepting it makes that commit the node result; the clean-tree rule
        // below applies only when no preserved commit exists.
        if let (Some(name), Some(commit)) = (&attempt.preserved_ref, &attempt.preserved_commit) {
            if !custody::ref_points_at(&execution.repo_root, name, commit) {
                return Err(resolution_error(
                    "preservation_point_unverified",
                    "the preservation ref is missing or moved; inspect before accepting",
                    None,
                    None,
                ));
            }
            // #2621: a verified node settles only on its own receipt for
            // exactly this commit. Bind it now, so land admits the accepted
            // commit; with none, refuse before the execution reaches a land
            // that cannot pass admission.
            let output = Self::preserved_verification(store, execution, attempt, commit)?;
            let pin = node_pin_ref(execution.id, &attempt.node_id, attempt.iteration);
            custody::pin_commit(&execution.repo_root, &pin, commit)?;
            return Ok(ResolutionWrite {
                execution_id: execution.id,
                attempt,
                expected_row_version: params.expected_row_version,
                idempotency_key: &params.idempotency_key,
                action: params.action.as_str(),
                attempt_status: AttemptStatus::Succeeded,
                resolution: Some("accepted"),
                failure_class: None,
                detail: json!({
                    "result_commit": commit,
                    "preserved_commit": commit,
                }),
                result_commit: Some(commit.clone()),
                pin_ref: Some(pin),
                output,
                review_assignment: None,
                retry: None,
                event_kind: "preserved_work_accepted",
                fingerprint: "",
                resume: true,
                actor,
            });
        }
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
            output: None,
            review_assignment: None,
            retry: None,
            event_kind: "preserved_work_accepted",
            fingerprint: "",
            resume: true,
            actor,
        })
    }

    /// #2621: the node output an accepted preserved commit settles with. A
    /// `require_verified` node carries the session's own `scoped_test`
    /// receipt for exactly `commit` (the check `AgentCheckLandingCandidate`
    /// makes), as `verification`, the field land admission reads; any other
    /// node records no output, as before. No such receipt refuses the accept.
    fn preserved_verification(
        store: &Store,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        commit: &str,
    ) -> Result<Option<serde_json::Value>> {
        let shape = crate::topology::graph::GraphShape::from_workflow(&execution.definition)?;
        if !shape.steps().require_verified(&attempt.node_id) {
            return Ok(None);
        }
        let jobs: Vec<_> = store
            .list_agent_jobs(attempt.session_id, crate::topology::verify::JOBS_READ)?
            .into_iter()
            .map(|row| row.job)
            .collect();
        let signatures = match execution.project_id {
            Some(project) => store.open_failure_signature_issues(project)?,
            None => Vec::new(),
        };
        match crate::topology::verify::judge(
            &jobs,
            attempt.session_id,
            commit,
            &crate::topology::verify::known_failures(&signatures),
        ) {
            Ok(evidence) => Ok(Some(serde_json::to_value(
                crate::topology::executor::node_data(&json!({
                    "verification": evidence.to_json(None),
                    "result_commit": commit,
                })),
            )?)),
            Err(unverified) => Err(resolution_error(
                "topology_preserved_unverified",
                &format!(
                    "no passing scoped_test receipt of the attempt's session verifies the \
                     preserved commit {commit} ({}: {}); retry the attempt so the node \
                     verifies it, or run a scoped_test job as the attempt's session at that \
                     commit and accept again",
                    unverified.reason.as_str(),
                    unverified.detail
                ),
                None,
                None,
            )),
        }
    }

    /// `retry`: only from a verified preservation point, as a new `attempt_no`
    /// forked from `preserved_commit`. The old sandbox and ref are kept. A
    /// blocked review attempt retries as a fresh review of the same commit
    /// (#2280): it has no preservation point to verify.
    fn retry_write<'a>(
        store: &Store,
        execution: &ExecutionRow,
        attempt: &'a AttemptRow,
        params: &'a ResolveTopologyAttemptParams,
        actor: Actor,
    ) -> Result<ResolutionWrite<'a>> {
        let review = attempt.node_kind == "review";
        let land = attempt.node_kind == "land";
        let attempts = rows::load_attempts(store, execution.id)?;
        let charged = attempts
            .iter()
            .filter(|row| row.node_id == attempt.node_id && row.iteration == attempt.iteration)
            .filter(|row| {
                !matches!(
                    row.failure_class.as_deref(),
                    Some(failure::LOST_BEFORE_SESSION | failure::REVIEW_INFRASTRUCTURE)
                )
            })
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
                if review {
                    "accept the review with a recorded ruling, or interrupt the execution"
                } else if land {
                    "interrupt the execution and land the accepted commit by hand"
                } else {
                    "accept or discard the preserved work instead"
                },
                None,
                None,
            ));
        }
        // A review or land retry re-runs the same commit: neither holds a
        // preservation point to verify.
        let preserved_commit = if review || land {
            attempt.base_commit.clone()
        } else {
            let verified = match (&attempt.preserved_ref, &attempt.preserved_commit) {
                (Some(name), Some(commit)) => {
                    custody::ref_points_at(&execution.repo_root, name, commit)
                }
                _ => false,
            };
            let Some(preserved_commit) = attempt.preserved_commit.clone().filter(|_| verified)
            else {
                return Err(resolution_error(
                    "preservation_point_unverified",
                    "the preservation ref is missing or moved; inspect before retrying",
                    None,
                    None,
                ));
            };
            preserved_commit
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
            output: None,
            review_assignment: None,
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
            event_kind: if review {
                "review_retried"
            } else if land {
                "land_retried"
            } else {
                "preserved_work_retried"
            },
            fingerprint: "",
            resume: true,
            actor,
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
    actor: Actor,
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
        output: None,
        review_assignment: None,
        detail: json!({
            "actor_kind": actor.kind,
            "actor_session_id": actor.session_id,
            "preserved_commit": attempt.preserved_commit,
            "preserved_ref": attempt.preserved_ref,
        }),
        retry: None,
        event_kind: "preserved_work_discarded",
        fingerprint: "",
        resume: false,
        actor,
    })
}
