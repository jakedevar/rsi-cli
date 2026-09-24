//! Runtime adapter for the durable manager intent and answer journals.

use std::sync::atomic::Ordering;

use rsi_common::harness_manager_v2::{
    AnswerHarnessManagerDecisionRequestV2, ManagerMutationReceiptV2,
};
use serde_json::Value;
use uuid::Uuid;

use super::SessionManager;
use crate::bus::DaemonEvent;
use crate::error::Result;
use crate::store::harness_manager_v2::refused;
use crate::store::manager_coordinator::ManagerDecisionDeliveryV2;

/// Policy-less V1 manager seats observed per coordinator pass (#669).
pub(crate) const MANAGER_SEAT_V1_PAGE: usize = 32;

impl SessionManager {
    /// Only the operator RPC calls this adapter. Admission records the exact
    /// answer and its delivery obligation; it never clears a question early.
    pub async fn answer_harness_manager_decision(
        &self,
        request: AnswerHarnessManagerDecisionRequestV2,
    ) -> Result<ManagerMutationReceiptV2> {
        let receipt = {
            let store = self.store.lock().await;
            store.manager_v2_prepare_decision_answer(&request, |config, key, version, digest| {
                store.manager_v2_apply_operator_acceptance(config, key, version, digest)
            })?
        };
        self.event_bus.publish(DaemonEvent::SystemMessage {
            level: "info".into(),
            message: format!(
                "Harness manager decision {}: operator answer recorded; inspect delivery state.",
                request.decision_key
            ),
        });
        Ok(receipt)
    }

    pub async fn reconcile_harness_managers_startup(&self) -> Result<usize> {
        let recovered = self
            .store
            .lock()
            .await
            .manager_v2_recover_decision_deliveries(self.program_run_boot_id)?;
        let actions = self.reconcile_manager_actions_startup().await?;
        // A seat attempt claimed by an earlier boot has no owner now. It is
        // made uncertain, never relaunched (#669).
        let seats = self
            .store
            .lock()
            .await
            .recover_manager_seat_claims(Some(self.program_run_boot_id))?;
        Ok(recovered + actions + seats + self.reconcile_harness_managers_once().await?)
    }

    /// One cursor/guard serializes startup, event hints and periodic backstop.
    /// The cursor is only a bounded traversal hint; all ownership is durable.
    pub async fn reconcile_harness_managers_once(&self) -> Result<usize> {
        let Ok(mut cursor) = self.manager_coordinator_cursor.try_lock() else {
            return Ok(0);
        };
        let projects = self
            .store
            .lock()
            .await
            .manager_v2_projects_after(*cursor, 8)?;
        let closure_changes = self.retry_appserver_approval_closures().await?;
        let mut changed = closure_changes + {
            let live = super::pending_approvals::live_incarnations();
            let store = self.store.lock().await;
            store.expire_appserver_approvals(&live)?
                + store.manager_v2_recover_decision_deliveries(self.program_run_boot_id)?
                + store.recover_manager_actions_startup(self.program_run_boot_id)?
        };
        for project in &projects {
            let reconciled = {
                let store = self.store.lock().await;
                store.reconcile_manager_intent(
                    *project,
                    self.runtime_config.retry_enabled.load(Ordering::Relaxed),
                )
            };
            match reconciled {
                Ok(result) => {
                    changed += result.changed;
                    if result.changed > 0 {
                        self.event_bus.publish(DaemonEvent::SystemMessage {level:if result.blocked>0 {"warn"} else {"info"}.into(),message:format!("Harness manager {project}: {} unfinished deliverables, {} blocked Epics, {} recovery actions admitted. Read the manager board for evidence and next actions.",result.unfinished,result.blocked,result.queued)});
                    }
                    self.store
                        .lock()
                        .await
                        .manager_v2_coordinator_error(*project, None)?;
                }
                Err(error) => {
                    // One malformed/oversized project must not starve others.
                    tracing::warn!(project_id=%project,error=%error,"manager intent reconciliation deferred");
                    let new_evidence = self
                        .store
                        .lock()
                        .await
                        .manager_v2_coordinator_error(*project, Some(&error.to_string()))?;
                    if new_evidence {
                        self.event_bus.publish(DaemonEvent::SystemMessage {
                            level: "warn".into(),
                            message: format!("Harness manager {project} needs attention: {error}"),
                        });
                    }
                }
            }
            // Boxed: the seat executor must not grow the coordinator future.
            changed += Box::pin(self.reconcile_manager_seat(*project)).await;
            let notice_config = self
                .store
                .lock()
                .await
                .get_harness_manager_notice_config(*project)?;
            if let Some(config) = notice_config {
                if let Err(error) = self
                    .store
                    .lock()
                    .await
                    .manager_v2_reconcile_notices(&config)
                {
                    tracing::warn!(project_id=%project,error=%error,"manager notice deferred; durable board retains obligation");
                }
            }
            let config = self.store.lock().await.get_harness_manager(*project)?;
            if let Some(config) = config {
                for _ in 0..4 {
                    let delivery = self
                        .store
                        .lock()
                        .await
                        .manager_v2_claim_decision_delivery(&config, self.program_run_boot_id)?;
                    let Some(delivery) = delivery else { break };
                    let outcome = self.deliver_manager_decision(delivery.clone()).await;
                    let store = self.store.lock().await;
                    let Some(record) =
                        store.manager_v2_record(&config, "decision_delivery", &delivery.key)?
                    else {
                        continue;
                    };
                    let current: ManagerDecisionDeliveryV2 =
                        serde_json::from_value(record.payload)?;
                    if current.state != "running" {
                        changed += 1;
                        continue;
                    }
                    match outcome {
                        Ok(()) => {
                            store.manager_v2_set_decision_delivery(&current,"delivered",true,Some("provider continuation established for the exact operator answer".into()))?;
                        }
                        Err(error) => {
                            let state = if current.effect_started {
                                "uncertain"
                            } else {
                                "blocked"
                            };
                            store.manager_v2_set_decision_delivery(
                                &current,
                                state,
                                current.effect_started,
                                Some(error.to_string()),
                            )?;
                        }
                    }
                    changed += 1;
                }
            }
        }
        // #669: V1 appointments without the V2 opt-in are outside the V2
        // traversal above; still observe and signal their seat (recovery
        // stays disabled without a policy). One page per pass behind a
        // wrapping cursor: every such seat is observed within
        // ceil(n / MANAGER_SEAT_V1_PAGE) passes.
        {
            let mut v1_cursor = self.manager_seat_v1_cursor.lock().await;
            let v1_projects = self
                .store
                .lock()
                .await
                .manager_seat_v1_projects_after(*v1_cursor, MANAGER_SEAT_V1_PAGE)?;
            *v1_cursor = if v1_projects.len() < MANAGER_SEAT_V1_PAGE {
                None
            } else {
                v1_projects.last().copied()
            };
            drop(v1_cursor);
            for project in v1_projects {
                changed += Box::pin(self.reconcile_manager_seat(project)).await;
            }
        }
        *cursor = if projects.len() < 8 {
            None
        } else {
            projects.last().copied()
        };
        // Lifecycle journal claims are globally single-flight and perform their
        // own guard-time checks. No second retry timer is installed here.
        changed += self.reconcile_manager_actions_once().await?;
        Ok(changed)
    }

    /// Issue #669: classify the appointed manager seat, publish a
    /// `[manager-seat]` `SystemMessage` on each state change, and execute at
    /// most one claimed in-place resume of the SAME tip row and sandbox.
    /// Caller holds the coordinator single-flight cursor. Errors are logged:
    /// one project's seat must not starve the others.
    pub(super) async fn reconcile_manager_seat(&self, project: Uuid) -> usize {
        match Box::pin(self.reconcile_manager_seat_inner(project)).await {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(project_id=%project,error=%error,"manager seat reconciliation deferred");
                0
            }
        }
    }

    async fn reconcile_manager_seat_inner(&self, project: Uuid) -> Result<usize> {
        let active: std::collections::HashSet<Uuid> =
            self.active.read().await.keys().copied().collect();
        let retry_enabled = self.runtime_config.retry_enabled.load(Ordering::Relaxed);
        let pass = {
            let store = self.store.lock().await;
            // The cursor excludes every other pass, so no live executor owns
            // a running claim here; an abandoned one becomes uncertain.
            let mut changed = store.recover_manager_seat_claims(None)?;
            let pass = store.reconcile_manager_seat(
                project,
                |tip| active.contains(&tip),
                retry_enabled,
                self.program_run_boot_id,
                chrono::Utc::now(),
            )?;
            drop(store);
            changed += pass.changed;
            (pass, changed)
        };
        let (pass, mut changed) = pass;
        if let Some((level, message)) = pass.notice {
            self.event_bus
                .publish(DaemonEvent::SystemMessage { level, message });
        }
        let Some(claim) = pass.claim else {
            return Ok(changed);
        };
        changed += Box::pin(self.execute_manager_seat_claim(project, claim)).await?;
        Ok(changed)
    }

    /// Execute one exact seat claim through the gated `ManagerSeat`
    /// continuation and settle it. Refusals at the boundary are typed
    /// non-success outcomes; only an established continuation succeeds.
    pub(crate) async fn execute_manager_seat_claim(
        &self,
        project: Uuid,
        claim: crate::store::manager_intent::manager_seat::ManagerSeatClaimV1,
    ) -> Result<usize> {
        let prompt = rsi_common::daemon_message::wrap(
            "manager-seat",
            &format!(
                "Your previous turn ended Failed, so rsid resumed this appointed manager seat in place (automatic recovery attempt {}/{} under the operator's persisted policy). Retrieve AgentManagerInbox and AgentManagerProgress, then continue your persisted duties. Pending lead mail and notices were retained while the seat was down.",
                claim.attempt, claim.max_attempts
            ),
        );
        // Same row, same sandbox_root, exact tip: the gate refuses a busy,
        // rotated, paused or otherwise ineligible tip before any launch.
        let outcome = Box::pin(self.continue_manager_seat(claim.clone(), prompt))
            .await
            .map_err(|error| match error {
                crate::error::DaemonError::InvalidParam(code) => code,
                other => other.to_string(),
            });
        if let Err(error) = &outcome {
            tracing::warn!(project_id=%project,session_id=%claim.tip_session_id,error=%error,"manager seat recovery attempt did not establish a continuation");
        }
        Ok(usize::from(
            self.store.lock().await.finish_manager_seat_claim(
                &claim,
                outcome.as_ref().copied().map_err(String::as_str),
            )?,
        ))
    }

    pub(super) async fn check_manager_decision_runtime(
        &self,
        delivery: &ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        let target = decision_session(delivery)?;
        // Match the actual latest question event, including an unpersisted
        // event whose id is still zero. Hold runtime read guards through the
        // durable check so detection+append cannot interleave between them.
        let matches_runtime = |pending: &Option<rsi_common::types::PendingQuestion>,
                               events: &[rsi_common::types::ConversationEvent]|
         -> Result<()> {
            let event = events.iter().rev().find(|event| {
                event.event_type == rsi_common::types::EventType::ToolUse
                    && event.tool_name.as_deref() == Some("AskUserQuestion")
            });
            let exact = event.is_some_and(|event| {
                event.id > 0
                    && delivery.target["event_id"].as_i64() == Some(event.id)
                    && delivery.target["event_sequence"].as_i64() == Some(i64::from(event.sequence))
                    && delivery.target["tool_use_id"].as_str() == event.tool_use_id.as_deref()
            });
            if !exact || serde_json::to_value(pending)? != delivery.target["question"] {
                return Err(refused("manager_v2_decision_runtime_changed"));
            }
            Ok(())
        };
        let active = self.active.read().await;
        let completed = self.completed.read().await;
        if let Some(tracked) = active.get(&target) {
            matches_runtime(&tracked.pending_question, &tracked.events)?;
        } else if let Some(cached) = completed.get(&target) {
            // Cold startup placeholders contain no runtime evidence; the
            // producer-bound durable publication is authoritative there.
            if cached.events_hydrated {
                matches_runtime(&cached.session.pending_question, &cached.events)?;
            }
        }
        self.store
            .lock()
            .await
            .manager_v2_check_decision_target(delivery)?;
        Ok(())
    }

    /// The caller holds the spawn guard and has established the provider, but
    /// has not installed its event monitor. No newer question can be consumed
    /// while this exact old question is cleared.
    pub(super) async fn clear_delivered_manager_question(
        &self,
        delivery: &ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        let target = decision_session(delivery)?;
        self.check_manager_decision_runtime(delivery).await?;
        self.store
            .lock()
            .await
            .clear_pending_question_exact(target, &delivery.target)
    }
}

pub(super) fn decision_session(delivery: &ManagerDecisionDeliveryV2) -> Result<Uuid> {
    delivery
        .target
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| refused("manager_v2_decision_target_required"))
}
