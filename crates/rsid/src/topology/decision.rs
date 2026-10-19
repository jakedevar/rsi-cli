//! Questions a topology node puts to the on-call manager (#1641 S3c).
//!
//! Two situations park a node on a decision record instead of failing or
//! blocking the execution:
//!
//! - a session node ends with a BLOCKED (or `human_gate`) handoff that names a
//!   `blocker_question`: the session stays as it is, the attempt goes
//!   `waiting`, and the ruling's answer continues the SAME session;
//! - a review node runs out of rounds with changes still requested: the
//!   manager chooses to accept as is, reopen the review once, or stop.
//!
//! The record is an ordinary decision on the project manager's ledger under a
//! `topology:` key (not a reserved gate key). Its `gate` follows the node's
//! blocker class, so a production, destructive, spend or credential question
//! is the operator's, and everything else is the on-call manager's to rule on.
//! The executor re-reads the record from the ledger every tick, so a restart
//! loses nothing; the record is the only state besides the attempt marker.
//!
//! Without a project-manager ledger that covers the execution's Epic there is
//! nowhere to file a record, and the caller falls back to the old behaviour
//! (a blocked attempt).

use rsi_common::agent_contract::{PipelineBlockerClassV1, StrictPipelineHandoffV2};
use rsi_common::harness_manager::HarnessManagerConfigV1;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::Result;
use crate::store::manager_decisions::{
    TOPOLOGY_DECISION_PREFIX, TopologyDecisionRequest, topology_decision_digest,
};
use crate::topology::executor::{AnswerContinuation, Executor, NodeEffects};
use crate::topology::review::{ExtraRound, OncallAcceptance};
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, Settlement, failure,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

/// Questions one attempt may ask before it blocks.
pub(crate) const MAX_QUESTIONS_PER_ATTEMPT: u32 = 3;

/// Failed deliveries of an answer before the attempt blocks visibly.
const MAX_DELIVERY_FAILURES: u64 = 3;

/// Marker kinds stored in a parked attempt's `output_json`.
pub(crate) const KIND_HANDOFF_QUESTION: &str = "handoff_question";
pub(crate) const KIND_REVIEW_EXHAUSTED: &str = "review_exhausted";

/// The gate class a blocked handoff's `blocker_class` declares, in the words
/// the decision classifier knows. `None` is a question the on-call manager may
/// rule on.
pub(crate) const fn blocker_gate(class: Option<PipelineBlockerClassV1>) -> Option<&'static str> {
    match class {
        Some(PipelineBlockerClassV1::Production) => Some("main_or_release"),
        Some(PipelineBlockerClassV1::Destructive) => Some("data_deletion"),
        Some(PipelineBlockerClassV1::Authority) => Some("credentials"),
        Some(PipelineBlockerClassV1::Resource) => Some("spend"),
        Some(PipelineBlockerClassV1::ForbiddenScope | PipelineBlockerClassV1::TechnicalImpasse)
        | None => None,
    }
}

/// Resource is operator-owned unless it explicitly names tooling and carries
/// no money signal. Keep this wider vocabulary out of ordinary manager asks.
fn handoff_blocker_gate(class: Option<PipelineBlockerClassV1>, text: &str) -> Option<&'static str> {
    let declared = if class == Some(PipelineBlockerClassV1::Resource) {
        let lower = text.to_lowercase();
        let words: Vec<_> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        // Match complete words/phrases, allowing punctuation such as cargo-slot.
        let normalized = format!(" {} ", words.join(" "));
        let tooling = [
            "tool copy",
            "stale binary",
            "stale tool",
            "redeploy rsid",
            "restart rsid",
            "cargo slot",
            "target pool",
            "build cache",
            "target dir",
            "disk",
            "sandbox",
            "worktree",
            "installed tools",
            "deploy",
        ]
        .iter()
        .any(|phrase| normalized.contains(&format!(" {phrase} ")));
        // Currency words/codes are conservative signals even without an amount;
        // this also covers either order around an amount. Symbols and the old
        // buy/pay/rent vocabulary remain covered by scan_decision_gate below.
        let money = words.iter().any(|word| {
            [
                "usd", "eur", "gbp", "jpy", "dollar", "dollars", "euro", "euros", "pound",
                "pounds", "cost", "costs", "costing", "price", "priced", "paid", "charge",
                "charged", "credit", "credits", "billed",
            ]
            .contains(word)
        });
        if tooling && !money {
            None
        } else {
            Some("spend")
        }
    } else {
        blocker_gate(class)
    };
    declared.or_else(|| crate::store::manager_decisions::scan_decision_gate(text))
}

/// `topology:<execution>:<node>:<iteration>:<attempt>` for the first question
/// of an attempt, with `:q<n>` appended for the n-th (n >= 2).
pub(crate) fn decision_key(execution: Uuid, attempt: &AttemptRow, question: u32) -> String {
    let base = format!(
        "{TOPOLOGY_DECISION_PREFIX}{execution}:{}:{}:{}",
        attempt.node_id, attempt.iteration, attempt.attempt_no
    );
    if question <= 1 {
        base
    } else {
        format!("{base}:q{question}")
    }
}

/// The decision a parked attempt waits on, read from its marker.
pub(crate) fn marker_key(attempt: &AttemptRow) -> Option<&str> {
    if attempt.status != AttemptStatus::Waiting {
        return None;
    }
    attempt.output.as_ref()?.get("decision_key")?.as_str()
}

/// What the ledger says about a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DecisionState {
    /// Nobody has ruled. `gated` records the operator-only case.
    Pending { gated: bool },
    /// Ruled or answered; the text is the answer.
    Answered(String),
    /// Withdrawn, archived, revoked or missing: the question will not be
    /// answered.
    Closed(String),
}

/// The ledger a decision lives on and the Epic that owns it.
struct Ledger {
    config: HarnessManagerConfigV1,
    epic: Uuid,
}

/// The on-call manager's three answers to an exhausted review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExhaustionRuling {
    /// Accept the reviewed commit as is.
    Accept,
    /// Reopen the review once.
    Round,
    /// Stop: block the execution as before.
    Stop,
}

impl ExhaustionRuling {
    /// The first word of the answer, case-insensitively. Anything that is not
    /// exactly `accept` or `round` is `stop`, the conservative reading.
    pub(crate) fn parse(answer: &str) -> Self {
        let word = answer
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        match word.as_str() {
            "accept" | "accepted" => Self::Accept,
            "round" => Self::Round,
            _ => Self::Stop,
        }
    }
}

/// The operator-facing reason no further review round exists. The decision
/// classifier reads the question text for gate words, so the ledger's machine
/// code (`..._budget`) is kept in the marker and never put in the question.
fn round_unavailable_text(reason: &str) -> &'static str {
    match reason {
        "manager_review_round_budget" => "the review has used all the rounds the policy allows",
        "manager_review_closure_specialist_required" => {
            "the closing round needs a different reviewer model and the policy allows none"
        }
        _ => "the review policy does not allow another round",
    }
}

/// Whether the execution's absolute deadline, if it has one, has passed.
pub(super) fn execution_deadline_passed(execution: &ExecutionRow) -> bool {
    execution
        .deadline_at
        .is_some_and(|deadline| chrono::Utc::now() >= deadline)
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

fn answer_delivery_marker(delivery_id: Uuid) -> String {
    format!("[Topology answer delivery {delivery_id}]")
}

fn answer_prompt(answer: &str, delivery_id: Uuid) -> String {
    format!(
        "My answer to your question: {answer}\n\n{}",
        answer_delivery_marker(delivery_id)
    )
}

/// Bind the exact prompt before the continuation claims delivery. Its unique
/// reference survives the claim and identifies the user event of this turn.
/// The continue path persists that event only after spawning the provider.
pub(super) fn bind_answer_continuation(
    store: &crate::store::Store,
    binding: &rows::AnswerBinding,
    session_id: Uuid,
    answer: &str,
) -> Result<String> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    rows::answer_delivery_live(store, binding, session_id)?;
    let text: String = tx.query_row(
        "SELECT output_json FROM topology_node_attempts WHERE id=?1",
        [binding.attempt_id.to_string()],
        |row| row.get(0),
    )?;
    let mut marker: Value = serde_json::from_str(&text)?;
    if marker["delivery"]["state"] == rows::DELIVERY_STARTED {
        return Err(crate::error::DaemonError::PolicyDenied(
            rows::ANSWER_UNCERTAIN.into(),
        ));
    }
    let delivery_id = Uuid::new_v4();
    marker["answer_delivery_id"] = json!(delivery_id);
    rows::update_decision_marker(store, binding.attempt_id, &marker)?;
    tx.commit()?;
    Ok(answer_prompt(answer, delivery_id))
}

/// A live session, another assistant message or an admission alone says
/// nothing about this answer. Require the user-turn event with this delivery's
/// unique marker, which survives continuation wrappers. Old claims without a reference
/// have no correlated evidence and remain uncertain.
fn answer_turn_observed(
    store: &crate::store::Store,
    session_id: Uuid,
    marker: &Value,
) -> Result<bool> {
    let Some(delivery_id) = marker["answer_delivery_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return Ok(false);
    };
    Ok(store
        .conn
        .query_row(
            "SELECT 1 FROM conversation_events WHERE session_id=?1
           AND event_type='Message' AND role='User' AND instr(content,?2)>0 LIMIT 1",
            params![session_id.to_string(), answer_delivery_marker(delivery_id)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

impl<E: NodeEffects> Executor<E> {
    /// The project manager's ledger for this execution, when the execution
    /// has a project and an Epic that ledger covers.
    async fn decision_ledger(&self, execution: &ExecutionRow) -> Result<Option<Ledger>> {
        let (Some(project), Some(epic)) = (execution.project_id, execution.epic_id) else {
            return Ok(None);
        };
        let store = self.store.lock().await;
        Ok(store
            .get_harness_manager(project)?
            .filter(|config| config.epic_ids.contains(&epic))
            .map(|config| Ledger { config, epic }))
    }

    /// File (or recognise) the decision for question `n` of this attempt and
    /// return its key. A record already pending for the same `source_digest`
    /// is this question after a restart; a record for something else (an
    /// earlier question) moves to the next ordinal. `None`: no ledger, or the
    /// attempt used all its questions.
    pub(crate) async fn file_decision(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        question: &str,
        context: String,
        gate: Option<&str>,
        source_digest: &str,
    ) -> Result<Option<String>> {
        let Some(ledger) = self.decision_ledger(execution).await? else {
            return Ok(None);
        };
        let store = self.store.lock().await;
        for ordinal in 1..=MAX_QUESTIONS_PER_ATTEMPT {
            let key = decision_key(execution.id, attempt, ordinal);
            match store.manager_v2_record(&ledger.config, "decision", &key)? {
                Some(record)
                    if record.payload["status"] == "pending"
                        && record.payload["target_digest"]
                            == topology_decision_digest(&key, source_digest)? =>
                {
                    return Ok(Some(key));
                }
                Some(_) => {}
                None => {
                    store.manager_v2_put_topology_decision(
                        &ledger.config,
                        &TopologyDecisionRequest {
                            key: key.clone(),
                            epic: ledger.epic,
                            question: question.to_owned(),
                            context: Some(clip(&context, 2_000)),
                            gate: gate.map(str::to_owned),
                            source_digest: source_digest.to_owned(),
                            actor: json!({
                                "kind": "topology_executor",
                                "session_id": null,
                                "node_label": null,
                                "execution_id": execution.id,
                                "attempt_id": attempt.id,
                                "node_id": attempt.node_id,
                            }),
                        },
                    )?;
                    return Ok(Some(key));
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn decision_state(
        &self,
        execution: &ExecutionRow,
        key: &str,
    ) -> Result<DecisionState> {
        let Some(ledger) = self.decision_ledger(execution).await? else {
            return Ok(DecisionState::Closed(
                "no manager ledger covers the Epic".into(),
            ));
        };
        let store = self.store.lock().await;
        let Some(record) = store.manager_v2_record(&ledger.config, "decision", key)? else {
            return Ok(DecisionState::Closed("the decision record is gone".into()));
        };
        if record.archived {
            return Ok(DecisionState::Closed("the decision was archived".into()));
        }
        let status = record.payload["status"].as_str().unwrap_or_default();
        Ok(match status {
            "pending" => {
                let gated = store.manager_v2_decision_is_operator_gate(key, &record.payload);
                DecisionState::Pending { gated }
            }
            "answered" => DecisionState::Answered(
                record.payload["answer"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ),
            other => DecisionState::Closed(format!("the decision is {other}")),
        })
    }

    /// Withdraw a pending decision whose node no longer waits for it.
    pub(crate) async fn withdraw_decision(
        &self,
        execution: &ExecutionRow,
        key: &str,
        reason: &str,
    ) -> Result<()> {
        let Some(ledger) = self.decision_ledger(execution).await? else {
            return Ok(());
        };
        let store = self.store.lock().await;
        store.manager_v2_withdraw_topology_decision(&ledger.config, key, reason)?;
        Ok(())
    }

    /// The execution's absolute deadline passed while the attempt waited on a
    /// ruling. The node's own clock stays paused for the wait, but the
    /// execution deadline never is: withdraw the outstanding decision, close
    /// the wait and settle the attempt `timeout`, so neither an unanswered
    /// question nor a late answer can outlive the execution (#1715).
    async fn expire_decision_wait(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        key: &str,
        what: &str,
    ) -> Result<bool> {
        self.withdraw_decision(execution, key, "execution deadline passed")
            .await?;
        self.account_node_wait(execution, attempt, false, None, None)
            .await?;
        self.reconcile_on_call(execution, attempt, false).await?;
        self.settle(
            execution,
            attempt,
            AttemptStatus::Failed,
            crate::topology::executor::failed(
                failure::TIMEOUT,
                &format!("the execution deadline passed while {what}"),
            ),
        )
        .await?;
        Ok(true)
    }

    /// A BLOCKED handoff that names a question parks its attempt on a
    /// decision. Returns whether it parked (otherwise the caller blocks the
    /// attempt as before).
    pub(crate) async fn park_on_blocker_question(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        handoff: &StrictPipelineHandoffV2,
        content: &str,
    ) -> Result<bool> {
        let Some(question) = handoff.blocker_question.as_deref() else {
            return Ok(false);
        };
        if execution.status == ExecutionStatus::Cancelling {
            return Ok(false);
        }
        let digest = rows::digest(content);
        let gate_text = format!(
            "{}\n{}\n{question}",
            handoff.handoff.blocker.as_deref().unwrap_or_default(),
            handoff.blocker_evidence.as_deref().unwrap_or_default(),
        );
        let gate = handoff_blocker_gate(handoff.blocker_class, &gate_text);
        let context = format!(
            "Topology execution `{}`, node `{}` (iteration {}, attempt {}). Blocker class: {}. \
             Evidence: {}",
            execution.name,
            attempt.node_id,
            attempt.iteration,
            attempt.attempt_no,
            handoff
                .blocker_class
                .map_or_else(|| "none".to_owned(), |class| format!("{class:?}")),
            handoff.blocker_evidence.as_deref().unwrap_or("none given"),
        );
        let Some(key) = self
            .file_decision(execution, attempt, question, context, gate, &digest)
            .await?
        else {
            return Ok(false);
        };
        let marker = json!({
            "decision_key": key,
            "decision_kind": KIND_HANDOFF_QUESTION,
            "handoff_digest": digest,
            "gated": gate.is_some(),
            "delivery_failures": 0,
        });
        let updates = {
            let store = self.store.lock().await;
            rows::mark_decision_waiting(
                &store,
                execution.id,
                attempt,
                &marker,
                attempt.started_at.unwrap_or(execution.created_at),
            )?
        };
        self.publish(updates);
        if let Some(class) = gate {
            self.effects.notify_operator(
                "warn",
                format!(
                    "Topology execution {} ({}): node '{}' asks a {class} question that only you \
                     can answer; it is on the decisions board ({key}).",
                    execution.id, execution.name, attempt.node_id
                ),
            );
        }
        // The session is finished; its build cache need not outlive the wait.
        self.effects.reclaim(attempt.session_id).await;
        Ok(true)
    }

    /// Drive a session attempt parked on a decision.
    pub(crate) async fn drive_session_decision(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let Some(key) = marker_key(attempt).map(str::to_owned) else {
            self.settle(
                execution,
                attempt,
                AttemptStatus::Blocked,
                crate::topology::executor::failed(
                    failure::HANDOFF_BLOCKED,
                    "attempt waits without a decision marker",
                ),
            )
            .await?;
            return Ok(true);
        };
        if execution.status == ExecutionStatus::Cancelling {
            self.withdraw_decision(execution, &key, "execution cancelled")
                .await?;
            crate::topology::oncall::note_answer_wait(attempt.session_id, false);
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
        if execution_deadline_passed(execution) {
            return self
                .expire_decision_wait(
                    execution,
                    attempt,
                    &key,
                    "the node waited for a ruling on its question",
                )
                .await;
        }
        match self.decision_state(execution, &key).await? {
            DecisionState::Pending { gated } => {
                // The node is not running: pause its wall clock and show the
                // wait when the manager who should rule is not live.
                self.account_node_wait(execution, attempt, true, Some(chrono::Utc::now()), None)
                    .await?;
                self.reconcile_on_call(execution, attempt, !gated).await?;
                Ok(false)
            }
            DecisionState::Answered(answer) => {
                self.continue_after_answer(execution, attempt, &key, &answer)
                    .await
            }
            DecisionState::Closed(reason) => {
                self.account_node_wait(execution, attempt, false, None, None)
                    .await?;
                self.reconcile_on_call(execution, attempt, false).await?;
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    crate::topology::executor::failed(
                        failure::HANDOFF_BLOCKED,
                        &format!("blocker question not answered: {reason}"),
                    ),
                )
                .await?;
                Ok(true)
            }
        }
    }

    /// The ruling arrived: continue the same session with the answer, then
    /// move the attempt back to `running`.
    ///
    /// Delivery is at most once (#1715). The continuation durably claims the
    /// delivery (`delivery.state = "started"` in the attempt marker) before the
    /// provider effect and rechecks the exact execution/attempt binding there.
    /// A claim without its exact continued user-turn event is uncertain:
    /// shown, never replayed. With
    /// no claim the answer was never sent, whatever the session has done since
    /// (#1740).
    async fn continue_after_answer(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        key: &str,
        answer: &str,
    ) -> Result<bool> {
        let marker = attempt.output.clone().unwrap_or(Value::Null);
        let claimed = marker["delivery"]["state"] == rows::DELIVERY_STARTED;
        let observed = self.effects.session(attempt.session_id).await;
        if observed.is_none() {
            self.settle(
                execution,
                attempt,
                AttemptStatus::Lost,
                crate::topology::executor::failed(
                    failure::LOST_AFTER_SESSION,
                    "node session row disappeared while waiting for an answer",
                ),
            )
            .await?;
            return Ok(true);
        }
        // Only this delivery's turn proves the answer was sent (#1740).
        // Session status and assistant output can belong to unrelated turns.
        let continued = if !claimed {
            false
        } else {
            let store = self.store.lock().await;
            answer_turn_observed(&store, attempt.session_id, &marker)?
        };
        if claimed && !continued {
            return self
                .block_uncertain_delivery(
                    execution,
                    attempt,
                    "no user-turn event is tied to the claimed answer delivery",
                )
                .await;
        }
        if !continued {
            let binding = rows::AnswerBinding {
                execution_id: execution.id,
                attempt_id: attempt.id,
                decision_key: key.to_owned(),
            };
            let result = {
                let store = self.store.lock().await;
                bind_answer_continuation(&store, &binding, attempt.session_id, answer)
            };
            let result = match result {
                Ok(prompt) => {
                    self.effects
                        .continue_with_answer(AnswerContinuation {
                            session_id: attempt.session_id,
                            prompt,
                            binding,
                        })
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                // The attempt row, not the error, says whether the provider
                // effect may have started.
                let started = {
                    let store = self.store.lock().await;
                    rows::answer_delivery_started(&store, attempt.id)?
                };
                if started {
                    return self
                        .block_uncertain_delivery(execution, attempt, &error.to_string())
                        .await;
                }
                // A revoked binding (cancelled, past its deadline) is settled
                // by the next tick; it is not a failed delivery.
                if error.to_string().contains(rows::ANSWER_REVOKED) {
                    return Ok(false);
                }
                let failures = marker["delivery_failures"].as_u64().unwrap_or(0) + 1;
                if failures >= MAX_DELIVERY_FAILURES {
                    self.account_node_wait(execution, attempt, false, None, None)
                        .await?;
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Blocked,
                        crate::topology::executor::failed(
                            failure::HANDOFF_BLOCKED,
                            &format!("the answer could not be delivered: {error}"),
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
                tracing::warn!(%error, attempt = %attempt.id, "topology answer delivery deferred");
                let mut marker = marker;
                marker["delivery_failures"] = json!(failures);
                let store = self.store.lock().await;
                rows::update_decision_marker(&store, attempt.id, &marker)?;
                return Ok(false);
            }
        }
        self.account_node_wait(execution, attempt, false, None, None)
            .await?;
        self.reconcile_on_call(execution, attempt, false).await?;
        let update = {
            let store = self.store.lock().await;
            rows::resume_after_decision(&store, execution.id, attempt, key)?
        };
        self.publish([update]);
        Ok(true)
    }

    /// The answer's provider effect may have started and nothing shows it
    /// landed: block visibly and never send it again (#945).
    async fn block_uncertain_delivery(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        detail: &str,
    ) -> Result<bool> {
        self.account_node_wait(execution, attempt, false, None, None)
            .await?;
        self.reconcile_on_call(execution, attempt, false).await?;
        self.effects.notify_operator(
            "warn",
            format!(
                "Topology execution {} ({}): node '{}' was sent a ruling's answer but the daemon \
                 cannot tell whether the session received it ({detail}). It is not sent again; \
                 continue the session yourself if it did not.",
                execution.id, execution.name, attempt.node_id
            ),
        );
        self.settle(
            execution,
            attempt,
            AttemptStatus::Blocked,
            crate::topology::executor::failed(
                failure::HANDOFF_BLOCKED,
                &format!("{}: {detail}", rows::ANSWER_UNCERTAIN),
            ),
        )
        .await?;
        Ok(true)
    }

    /// A review that ran out of rounds asks the on-call manager what to do.
    /// Returns whether it parked (otherwise the caller blocks as before).
    /// `review_output` is the changes-requested output the attempt would have
    /// recorded; `error` its blocked-error text.
    pub(crate) async fn park_review_exhaustion(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        review_output: &Value,
        error: &str,
    ) -> Result<bool> {
        if execution.status == ExecutionStatus::Cancelling {
            return Ok(false);
        }
        // One reopened review per instance: the second exhaustion is final.
        let reopened = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
                .iter()
                .any(|other| {
                    other.node_id == attempt.node_id
                        && other.iteration == attempt.iteration
                        && other.failure_class.as_deref() == Some(failure::REVIEW_REOPENED)
                })
        };
        if reopened {
            return Ok(false);
        }
        // Offer `round` only when the review ledger would accept one (#1715):
        // the store's round budget and closing-reviewer rule are unchanged.
        let extra = self.review_extra_round(execution, attempt).await?;
        let question = match &extra {
            ExtraRound::Same => "The review ran out of rounds and still requests changes. \
                Answer with exactly one word: `accept` (take the commit as is), `round` (one \
                more review round) or `stop` (block the execution)."
                .to_owned(),
            ExtraRound::Closure(closing) => format!(
                "The review ran out of rounds and still requests changes. Answer with exactly \
                 one word: `accept` (take the commit as is), `round` (one more review round, \
                 which the review policy requires a different reviewer for: `{}`) or `stop` \
                 (block the execution).",
                closing.model
            ),
            ExtraRound::Unavailable(reason) => format!(
                "The review ran out of rounds and still requests changes. Answer with exactly \
                 one word: `accept` (take the commit as is) or `stop` (block the execution). \
                 One more review round is not available: {}.",
                round_unavailable_text(reason)
            ),
        };
        let context = format!(
            "Topology execution `{}`, review node `{}`. {error}",
            execution.name, attempt.node_id
        );
        let digest = rows::digest(&format!("{}:{error}", attempt.id));
        let Some(key) = self
            .file_decision(
                execution,
                attempt,
                &question,
                clip(&context, 8_000),
                None,
                &digest,
            )
            .await?
        else {
            return Ok(false);
        };
        let marker = json!({
            "decision_key": key,
            "decision_kind": KIND_REVIEW_EXHAUSTED,
            "review_output": review_output,
            "error": clip(error, 1_500),
            "round_unavailable": match &extra {
                ExtraRound::Unavailable(reason) => Some(reason.as_str()),
                _ => None,
            },
        });
        let updates = {
            let store = self.store.lock().await;
            rows::mark_decision_waiting(
                &store,
                execution.id,
                attempt,
                &marker,
                attempt.started_at.unwrap_or(execution.created_at),
            )?
        };
        self.publish(updates);
        Ok(true)
    }

    /// Drive a review attempt parked on an exhaustion decision.
    pub(crate) async fn drive_review_decision(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        key: &str,
    ) -> Result<bool> {
        let marker = attempt.output.clone().unwrap_or(Value::Null);
        let review_output = marker["review_output"].clone();
        let error = marker["error"].as_str().unwrap_or_default().to_owned();
        if execution_deadline_passed(execution) {
            return self
                .expire_decision_wait(
                    execution,
                    attempt,
                    key,
                    "the review waited for a ruling on its exhausted rounds",
                )
                .await;
        }
        let stop = |reason: String| {
            let error = format!("{error} ({reason})");
            Settlement {
                failure_class: Some(failure::REVIEW_ROUNDS_EXHAUSTED),
                error: Some(error),
                output: (!review_output.is_null()).then(|| review_output.clone()),
                ..Settlement::default()
            }
        };
        match self.decision_state(execution, key).await? {
            DecisionState::Pending { gated } => {
                self.reconcile_on_call(execution, attempt, !gated).await?;
                Ok(false)
            }
            DecisionState::Answered(answer) => {
                self.account_node_wait(execution, attempt, false, None, None)
                    .await?;
                self.reconcile_on_call(execution, attempt, false).await?;
                let ruling = ExhaustionRuling::parse(&answer);
                if ruling == ExhaustionRuling::Round
                    && let Some(reason) = marker["round_unavailable"].as_str()
                {
                    // The ruling asked for a round the review policy does not
                    // offer: stop, and say why, rather than reopen and fail.
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Blocked,
                        stop(format!("another review round is unavailable: {reason}")),
                    )
                    .await?;
                    return Ok(true);
                }
                match ruling {
                    ExhaustionRuling::Accept => {
                        // The acceptance is a review-ledger fact for this
                        // assignment and exact commit, recorded before the
                        // attempt settles, so the land node's admission
                        // re-check admits it (#1740). Without the fact the
                        // land would be refused; stop and say why instead.
                        let Some(assignment) = attempt.review_assignment_id else {
                            self.settle(
                                execution,
                                attempt,
                                AttemptStatus::Blocked,
                                stop("the exhausted review recorded no assignment".into()),
                            )
                            .await?;
                            return Ok(true);
                        };
                        if let Err(error) = self
                            .effects
                            .record_oncall_acceptance(OncallAcceptance {
                                execution_id: execution.id,
                                attempt_id: attempt.id,
                                node_id: attempt.node_id.clone(),
                                assignment_id: assignment,
                                commit: attempt.base_commit.clone(),
                                decision_key: key.to_owned(),
                            })
                            .await
                        {
                            self.settle(
                                execution,
                                attempt,
                                AttemptStatus::Blocked,
                                stop(format!("the acceptance could not be recorded: {error}")),
                            )
                            .await?;
                            return Ok(true);
                        }
                        let mut output = review_output.clone();
                        if let Some(fields) =
                            output.get_mut("fields").and_then(Value::as_object_mut)
                        {
                            fields.insert("verdict".into(), "accepted".into());
                            fields.insert("ruling".into(), "accepted_as_is".into());
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
                    }
                    ExhaustionRuling::Round => {
                        self.settle(
                            execution,
                            attempt,
                            AttemptStatus::Failed,
                            crate::topology::executor::failed(
                                failure::REVIEW_REOPENED,
                                "the on-call manager ruled one more review round",
                            ),
                        )
                        .await?;
                    }
                    ExhaustionRuling::Stop => {
                        self.settle(
                            execution,
                            attempt,
                            AttemptStatus::Blocked,
                            stop("the on-call manager ruled stop".into()),
                        )
                        .await?;
                    }
                }
                Ok(true)
            }
            DecisionState::Closed(reason) => {
                self.account_node_wait(execution, attempt, false, None, None)
                    .await?;
                self.reconcile_on_call(execution, attempt, false).await?;
                self.settle(execution, attempt, AttemptStatus::Blocked, stop(reason))
                    .await?;
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn exhaustion_answers_default_to_stop() {
        for (answer, ruling) in [
            ("accept", ExhaustionRuling::Accept),
            (
                "  Accept as is, the findings are cosmetic",
                ExhaustionRuling::Accept,
            ),
            ("ROUND", ExhaustionRuling::Round),
            ("round.", ExhaustionRuling::Round),
            ("stop", ExhaustionRuling::Stop),
            ("one more round please", ExhaustionRuling::Stop),
            ("", ExhaustionRuling::Stop),
        ] {
            assert_eq!(ExhaustionRuling::parse(answer), ruling, "{answer:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn resource_tooling_exception_requires_explicit_words_and_preserves_other_gates() {
        use PipelineBlockerClassV1::Resource;
        for tooling in [
            "tool copy",
            "stale binary",
            "stale tool",
            "redeploy rsid",
            "restart rsid",
            "cargo-slot",
            "target-pool",
            "build cache",
            "target dir",
            "disk",
            "sandbox",
            "worktree",
            "installed tools",
            "deploy",
        ] {
            assert_eq!(
                handoff_blocker_gate(Some(Resource), &tooling.to_uppercase()),
                None,
                "{tooling}"
            );
            assert_eq!(
                handoff_blocker_gate(Some(Resource), &format!("{tooling}: paid run")),
                Some("spend"),
                "{tooling}"
            );
        }
        for unknown in [
            "capacity unavailable",
            "tool unavailable",
            "deployment",
            "sandboxed",
            "worktrees",
        ] {
            assert_eq!(
                handoff_blocker_gate(Some(Resource), unknown),
                Some("spend"),
                "{unknown}"
            );
        }
        for (text, gate) in [
            ("redeploy rsid for $25", "spend"),
            ("redeploy rsid: API key needed", "credentials"),
            ("deploy to production", "main_or_release"),
            ("sandbox: delete user data", "data_deletion"),
        ] {
            assert_eq!(
                handoff_blocker_gate(Some(Resource), text),
                Some(gate),
                "{text}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn blocker_classes_map_to_gates() {
        use PipelineBlockerClassV1::*;
        assert_eq!(blocker_gate(Some(Production)), Some("main_or_release"));
        assert_eq!(blocker_gate(Some(Destructive)), Some("data_deletion"));
        assert_eq!(blocker_gate(Some(Resource)), Some("spend"));
        assert_eq!(blocker_gate(Some(Authority)), Some("credentials"));
        assert_eq!(blocker_gate(Some(ForbiddenScope)), None);
        assert_eq!(blocker_gate(Some(TechnicalImpasse)), None);
        assert_eq!(blocker_gate(None), None);
    }
}
