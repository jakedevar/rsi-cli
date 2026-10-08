//! Delivery of operator answers outside the manager inbox. The coordinator's
//! single-flight guard owns claims; provider guards own the final effect fence.
use super::SessionManager;
use crate::{
    error::{DaemonError, Result},
    store::pending_questions::remote_answers::RemoteAnswerDelivery,
};
use rsi_common::remote_pending_decisions::RemoteAnswerStateV1 as State;
use serde_json::json;

const MAX_PRE_EFFECT_RETRIES: u64 = 3;

fn retryable_pre_effect(error: &DaemonError) -> bool {
    matches!(error, DaemonError::InvalidParam(code) if matches!(code.as_str(),
        "manager_v2_approval_writer_capacity" |
        "manager_v2_approval_writer_unavailable" |
        "manager_v2_approval_runtime_changed" |
        "manager_v2_decision_runtime_changed"))
}

impl SessionManager {
    pub(super) async fn reconcile_remote_answers_once(&self) -> Result<usize> {
        let mut changed = 0;
        for _ in 0..4 {
            let delivery = self
                .store
                .lock()
                .await
                .claim_remote_answer(self.program_run_boot_id)?;
            let Some(delivery) = delivery else { break };
            let outcome = if delivery.target["kind"] == "appserver_approval" {
                self.deliver_remote_appserver_approval(delivery.clone())
                    .await
            } else {
                self.continue_remote_answer(delivery.clone()).await
            };
            let store = self.store.lock().await;
            let mut requeued = false;
            if let Some(current) = store.remote_answer_delivery(delivery.key())? {
                if current.receipt.state == State::Running {
                    let retries = current
                        .receipt
                        .outcome
                        .as_ref()
                        .and_then(|v| v["pre_effect_retries"].as_u64())
                        .unwrap_or(0);
                    let (state, detail) = match outcome {
                        Ok(()) => (State::Succeeded, json!({"code":"continuation_established"})),
                        Err(error)
                            if !current.effect_started
                                && retryable_pre_effect(&error)
                                && retries < MAX_PRE_EFFECT_RETRIES
                                && store.check_remote_answer_target(&current).is_ok() =>
                        {
                            requeued = true;
                            (
                                State::Queued,
                                json!({"code":"remote_answer_retry_pre_effect",
                                "pre_effect_retries":retries + 1, "detail":error.to_string()}),
                            )
                        }
                        Err(error) => (
                            if current.effect_started {
                                State::Uncertain
                            } else {
                                State::Refused
                            },
                            json!({"code":if current.effect_started {"remote_answer_effect_uncertain"}
                                else if retryable_pre_effect(&error) && retries >= MAX_PRE_EFFECT_RETRIES {"remote_answer_retry_exhausted"}
                                else {"decision_changed"},"detail":error.to_string()}),
                        ),
                    };
                    store.set_remote_answer_delivery(
                        &current,
                        state,
                        current.effect_started,
                        Some(detail),
                    )?;
                }
            }
            changed += 1;
            // One retry per coordinator pass; never burn the budget in this
            // four-delivery loop while capacity/runtime recovery is pending.
            if requeued {
                break;
            }
        }
        Ok(changed)
    }

    pub(super) async fn check_remote_answer_runtime(
        &self,
        delivery: &RemoteAnswerDelivery,
    ) -> Result<()> {
        self.check_question_runtime(delivery.session_id(), &delivery.target)
            .await?;
        self.store.lock().await.check_remote_answer_target(delivery)
    }

    pub(super) async fn clear_delivered_remote_question(
        &self,
        delivery: &RemoteAnswerDelivery,
    ) -> Result<()> {
        self.check_remote_answer_runtime(delivery).await?;
        self.store
            .lock()
            .await
            .clear_pending_question_exact(delivery.session_id(), &delivery.target)
    }
}

#[cfg(test)]
mod tests;
