//! K2 (#380/#390): effect observation for uncertain manager actions and the
//! execution of the two typed recovery actions. Observation never repeats an
//! effect; the spawn guards it takes are held until the settlement CAS.

use super::*;
use crate::store::manager_actions::{
    ManagerActionOperationV2, SETTLED_EFFECT_ABSENT, SETTLED_PAUSE_CONFIRMED,
};
use serde_json::{Value, json};

pub(super) enum EffectObservation {
    Proven(&'static str),
    Absent,
    Unobservable,
}

type Guards = Vec<super::super::spawn_single_flight::SpawnGuard>;

impl SessionManager {
    /// Same witness as `settle_manager_predecessor`: absent from the active
    /// map (caller holds the spawn guard), durably terminal when a row is
    /// required, and the daemon-owned process cohort reaped to a fixed point.
    async fn manager_session_quiescent(
        &self,
        session: Uuid,
        require_row: bool,
    ) -> Result<Option<Value>> {
        if self.active.read().await.contains_key(&session) {
            return Ok(None);
        }
        let status = self
            .store
            .lock()
            .await
            .get_session(session)?
            .map(|s| s.status);
        match status {
            Some(SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted) => {
            }
            None if !require_row => {}
            _ => return Ok(None),
        }
        let reaped = tokio::task::spawn_blocking(move || {
            super::super::reaper::reap_orphans_for_session(session)
        })
        .await;
        let Ok(Ok(reaped)) = reaped else {
            return Ok(None);
        };
        Ok(Some(json!({
            "session_id": session,
            "status": status,
            "active": false,
            "reaped_orphans": reaped,
        })))
    }

    /// Automatic reconciliation never waits behind a live launch: a contended
    /// guard leaves the action uncertain until a later pass.
    async fn recovery_guard(
        session: Uuid,
        automatic: bool,
    ) -> Option<super::super::spawn_single_flight::SpawnGuard> {
        if automatic {
            super::super::spawn_single_flight::try_acquire_spawn_guard(session)
        } else {
            Some(super::super::spawn_single_flight::acquire_spawn_guard(session).await)
        }
    }

    /// An assignment's lead CAS commits with its success receipt, so for
    /// pause and assign the only possible uncertain effect is interrupting the
    /// exact predecessor.
    async fn observe_predecessor_interrupt(
        &self,
        action: &ManagerActionV2,
        automatic: bool,
        guards: &mut Guards,
    ) -> Result<(EffectObservation, Value)> {
        use EffectObservation::{Absent, Proven, Unobservable};
        let pause = matches!(action, ManagerActionV2::PauseLead { .. });
        let candidate = match action {
            ManagerActionV2::AssignLead { session_id, .. } => *session_id,
            _ => None,
        };
        let predecessor = action_fence(action)
            .and_then(|f| f.lead_session_id)
            .filter(|id| Some(*id) != candidate);
        let Some(predecessor) = predecessor else {
            return Ok(if pause {
                (Unobservable, json!({"predecessor": null}))
            } else {
                (Absent, json!({"predecessor": null, "lead_cas": "absent"}))
            });
        };
        let Some(guard) = Self::recovery_guard(predecessor, automatic).await else {
            return Ok((Unobservable, json!({"predecessor": predecessor})));
        };
        guards.push(guard);
        let quiescent = self.manager_session_quiescent(predecessor, true).await?;
        Ok(match quiescent {
            Some(witness) if pause => (
                Proven(SETTLED_PAUSE_CONFIRMED),
                json!({"predecessor": witness}),
            ),
            Some(witness) => (
                Absent,
                json!({"predecessor": witness, "lead_cas": "absent"}),
            ),
            None => (Unobservable, json!({"predecessor": predecessor})),
        })
    }

    pub(super) async fn observe_uncertain_manager_effect(
        &self,
        op: &ManagerActionOperationV2,
        automatic: bool,
    ) -> Result<(EffectObservation, Value, Guards)> {
        use EffectObservation::{Absent, Proven, Unobservable};
        use ManagerActionV2::{
            ArchiveContainer, ArchiveSession, AssignLead, CreateContainer, CreateSession,
            DeleteContainer, Integrate, OperatorCall, PauseLead, ReplaceLead, RestoreContainer,
            RestoreSession, ResumeLead, RetireLeadContinuations, RetryLead, SettleUncertainAction,
            SucceedManager, UpdateContainer, UpdateSession,
        };
        let action = &op.context.request.operation;
        let mut guards = Guards::new();
        let observation = match action {
            PauseLead { .. } | AssignLead { .. } => {
                self.observe_predecessor_interrupt(action, automatic, &mut guards)
                    .await?
            }
            ResumeLead { .. } => {
                let target = op
                    .context
                    .target_session_id
                    .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
                let Some(guard) = Self::recovery_guard(target, automatic).await else {
                    return Ok((Unobservable, json!({"target": target}), guards));
                };
                guards.push(guard);
                let invocation: Option<String> = self.store.lock().await.conn.query_row(
                    "SELECT (SELECT status FROM model_invocations WHERE dedup_key=?1 AND session_id=?2 ORDER BY id LIMIT 1)",
                    rusqlite::params![format!("manager.action:{}", op.receipt.operation_id), target.to_string()],
                    |r| r.get(0),
                )?;
                let quiescent = self.manager_session_quiescent(target, true).await?;
                match (invocation.as_deref(), quiescent) {
                    (Some("completed"), Some(witness)) => (
                        Proven("lead_resumed_reconciled"),
                        json!({"target": witness, "invocation": "completed"}),
                    ),
                    (None, Some(witness)) => {
                        (Absent, json!({"target": witness, "invocation": null}))
                    }
                    (invocation, _) => (
                        Unobservable,
                        json!({"target": target, "invocation": invocation}),
                    ),
                }
            }
            CreateSession { .. } | ReplaceLead { .. } | RetryLead { .. } => {
                // Establishment and any lead CAS commit with the receipt; a
                // quiescent (or never-created) candidate proves no effect.
                let target = op
                    .context
                    .target_session_id
                    .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
                let Some(guard) = Self::recovery_guard(target, automatic).await else {
                    return Ok((Unobservable, json!({"target": target}), guards));
                };
                guards.push(guard);
                self.manager_session_quiescent(target, false)
                    .await?
                    .map_or_else(
                        || (Unobservable, json!({"candidate": target})),
                        |witness| {
                            (
                                Absent,
                                json!({"candidate": witness, "establishment": "absent"}),
                            )
                        },
                    )
            }
            CreateContainer { .. }
            | UpdateContainer { .. }
            | ArchiveContainer { .. }
            | DeleteContainer { .. }
            | RestoreContainer { .. }
            | SettleUncertainAction { .. }
            | RetireLeadContinuations { .. }
            | ArchiveSession { .. }
            | RestoreSession { .. }
            | UpdateSession { .. } => {
                // These effects commit in the same transaction as the success
                // receipt; an uncertain row therefore never committed them.
                (Absent, json!({"transactional_effect": "absent"}))
            }
            SucceedManager { .. } | Integrate { .. } | OperatorCall { .. } => {
                (Unobservable, json!({"owner": "dedicated_reconciler"}))
            }
        };
        Ok((observation.0, observation.1, guards))
    }

    /// #380: bounded daemon settlement of uncertain pause/assign actions whose
    /// exact predecessor is durably terminal and process-absent.
    pub(super) async fn reconcile_uncertain_lead_actions(&self) -> Result<usize> {
        let candidates = self
            .store
            .lock()
            .await
            .uncertain_manager_recovery_candidates()?;
        let mut count = 0;
        for op in candidates {
            let id = op.receipt.operation_id;
            let (observation, mut witness, guards) =
                match self.observe_uncertain_manager_effect(&op, true).await {
                    Ok(observed) => observed,
                    Err(error) => {
                        tracing::warn!(operation_id=%id, %error,
                            "uncertain manager action observation deferred");
                        continue;
                    }
                };
            let (state, outcome) = match observation {
                EffectObservation::Proven(outcome) => (ManagerActionStateV2::Succeeded, outcome),
                EffectObservation::Absent => (ManagerActionStateV2::Failed, SETTLED_EFFECT_ABSENT),
                EffectObservation::Unobservable => continue,
            };
            witness["reconciled_by"] = json!("daemon");
            let settled = self
                .store
                .lock()
                .await
                .reconcile_uncertain_manager_action(&op, state, outcome, &witness);
            drop(guards);
            match settled {
                Ok(Some(_)) => {
                    count += 1;
                    self.publish_manager_action_notice(id).await;
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(operation_id=%id, %error,
                    "uncertain manager action settlement deferred"),
            }
        }
        Ok(count)
    }

    pub(super) async fn execute_manager_settle(
        &self,
        claim: &ManagerActionClaimV2,
        operation_id: Uuid,
    ) -> Result<()> {
        let op = self
            .store
            .lock()
            .await
            .manager_action_operation(operation_id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        let (observation, witness, guards) =
            self.observe_uncertain_manager_effect(&op, false).await?;
        let (state, outcome) = match observation {
            EffectObservation::Proven(outcome) => (ManagerActionStateV2::Succeeded, outcome),
            EffectObservation::Absent => (ManagerActionStateV2::Failed, SETTLED_EFFECT_ABSENT),
            // The original stays uncertain; this request is refused.
            EffectObservation::Unobservable => {
                return Err(refused("manager_v2_effect_unobservable"));
            }
        };
        self.store
            .lock()
            .await
            .apply_manager_settle_action(claim, state, outcome, &witness)?;
        drop(guards);
        self.publish_manager_action_notice(operation_id).await;
        Ok(())
    }

    pub(super) async fn execute_manager_retire(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let lead = action_fence(claim.action())
            .and_then(|f| f.lead_session_id)
            .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
        // Held through the transaction: no continuation can start in between.
        let _guard = super::super::spawn_single_flight::acquire_spawn_guard(lead).await;
        self.check_manager_action_runtime(claim, false).await?;
        if self.active.read().await.contains_key(&lead) {
            return Err(refused("manager_v2_lead_active"));
        }
        self.store
            .lock()
            .await
            .apply_manager_retire_continuations(claim)
    }

    async fn publish_manager_action_notice(&self, operation_id: Uuid) {
        let queued = self
            .store
            .lock()
            .await
            .reconcile_manager_action_notice(operation_id);
        match queued {
            Ok(Some(job_id)) => self
                .event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id }),
            Ok(None) => {}
            Err(error) => tracing::warn!(%operation_id, %error,
                "manager action notice reconciliation deferred"),
        }
    }
}
