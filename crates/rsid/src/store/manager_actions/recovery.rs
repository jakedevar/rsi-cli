//! K2 (#380/#390): typed manager recovery for stale ownership.
//!
//! Settlement re-observes an uncertain action's effect elsewhere and only
//! appends evidence here: the original payload, context and execution records
//! are never rewritten and no effect is re-executed. Retirement disables a
//! terminal/idle lead's continuation owners and records an exact witness that
//! its agent-declared program outcome is superseded by manager recovery.

use super::*;
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};

impl Store {
    /// Scope, capability and version checks for `settle_uncertain_action`.
    /// The settled operation must belong to the caller's project and its
    /// Epic/container must still be in the current manager scope.
    pub(super) fn manager_settle_target(
        &self,
        authority: &ManagerAuthorityV2,
        operation_id: Uuid,
        expected_row_version: i64,
        check_version: bool,
    ) -> Result<()> {
        let op = self
            .manager_action_operation(operation_id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if op.project_id != authority.config.project_id {
            return Err(refused("manager_v2_action_out_of_scope"));
        }
        self.manager_settle_scope(authority, &op, 0)?;
        if check_version {
            if op.receipt.state != ManagerActionStateV2::Uncertain {
                return Err(refused("manager_v2_action_not_uncertain"));
            }
            if op.receipt.row_version != expected_row_version {
                return Err(refused("manager_v2_action_changed"));
            }
        }
        Ok(())
    }

    /// Every level of a (possibly nested) settlement must be in the caller's
    /// project, granted, and target an Epic/container in the current scope.
    /// A settlement of a settlement resolves and checks the inner operation.
    fn manager_settle_scope(
        &self,
        authority: &ManagerAuthorityV2,
        op: &ManagerActionOperationV2,
        depth: usize,
    ) -> Result<()> {
        if op.project_id != authority.config.project_id || depth > MAX_SETTLE_NESTING {
            return Err(refused("manager_v2_action_out_of_scope"));
        }
        let original = &op.context.request.operation;
        // LeadControl is checked by the shared authorizer; the original
        // action's own grant is required as well.
        if !authority
            .grant
            .policy
            .capabilities
            .contains(&original.capability())
        {
            return Err(refused("manager_v2_capability_denied"));
        }
        match original {
            // Root succession and integration own dedicated reconcilers.
            ManagerActionV2::SucceedManager { .. } | ManagerActionV2::Integrate { .. } => {
                return Err(refused("manager_v2_effect_unobservable"));
            }
            ManagerActionV2::SettleUncertainAction { operation_id, .. } => {
                let inner = self
                    .manager_action_operation(*operation_id)?
                    .ok_or_else(|| refused("manager_v2_action_out_of_scope"))?;
                return self.manager_settle_scope(authority, &inner, depth + 1);
            }
            _ => {}
        }
        if let Some(epic) = action_epic(original) {
            self.manager_v2_require_epic(authority, epic)?;
            return Ok(());
        }
        let container = match original {
            ManagerActionV2::CreateSession { parent_id, .. } => Some(*parent_id),
            ManagerActionV2::CreateContainer { parent_id, .. } => *parent_id,
            ManagerActionV2::UpdateContainer { container_id, .. }
            | ManagerActionV2::ArchiveContainer { container_id, .. }
            | ManagerActionV2::DeleteContainer { container_id, .. }
            | ManagerActionV2::RestoreContainer { container_id, .. } => Some(*container_id),
            _ => return Err(refused("manager_v2_action_out_of_scope")),
        };
        if let Some(id) = container {
            self.manager_v2_require_container(authority, id, true)?;
        }
        Ok(())
    }

    /// Dispatch-time owner check for a scheduled Resume wake or child watch,
    /// run under the target's spawn guard. The exact rows must still be
    /// enabled and must not have been retired by manager recovery; a stale
    /// due-list snapshot therefore cannot restart a retired lead.
    ///
    /// `target` is the RESOLVED delivery target after the rotation-lineage
    /// chase. A live exact retirement witness for it refuses every scheduled
    /// wake, whichever lineage member the job named (review round 2).
    pub(crate) fn check_scheduled_wake_owner(&self, target: Uuid, job_ids: &[Uuid]) -> Result<()> {
        for job in job_ids {
            let owned: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM scheduled_jobs j WHERE j.id=?1 AND j.enabled=1)
                   AND NOT EXISTS(SELECT 1 FROM harness_manager_v2_records r, json_each(r.payload_json,'$.disabled_job_ids') d
                     WHERE r.kind=?2 AND d.value=?1
                       AND r.record_key IN (?3,(SELECT wake_session_id FROM scheduled_jobs WHERE id=?1)))",
                params![job.to_string(), LEAD_RETIREMENT_KIND, target.to_string()],
                |r| r.get(0),
            )?;
            if !owned {
                return Err(refused("scheduled_wake_retired_or_disabled"));
            }
        }
        if self.manager_lead_program_outcome_superseded(target)? {
            return Err(refused("scheduled_wake_target_retired"));
        }
        Ok(())
    }

    /// Bounded daemon reconciliation input: uncertain actions whose only
    /// possible uncertain effect was interrupting their exact predecessor.
    pub(crate) fn uncertain_manager_recovery_candidates(
        &self,
    ) -> Result<Vec<ManagerActionOperationV2>> {
        let ids: Vec<String> = {
            let mut statement = self.conn.prepare(
                "SELECT id FROM harness_manager_v2_operations
                 WHERE kind=?1 AND state='uncertain'
                   AND json_extract(payload_json,'$.request.operation.action') IN ('pause_lead','assign_lead')
                 ORDER BY updated_at,id LIMIT ?2",
            )?;
            statement
                .query_map(params![ACTION_KIND, UNCERTAIN_RECOVERY_BATCH], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        let mut operations = Vec::with_capacity(ids.len());
        for id in ids {
            match self.manager_action_operation(parse_id(&id)?) {
                Ok(Some(operation)) => operations.push(operation),
                Ok(None) => {}
                // Unreadable legacy evidence stays uncertain; never guess.
                Err(error) => tracing::warn!(operation_id=%id, %error,
                    "uncertain manager action evidence is unreadable"),
            }
        }
        Ok(operations)
    }

    /// Daemon-owned settlement (no manager authority, no effect). A concurrent
    /// settlement or a changed row leaves the newer evidence untouched.
    pub(crate) fn reconcile_uncertain_manager_action(
        &self,
        operation: &ManagerActionOperationV2,
        state: ManagerActionStateV2,
        outcome: &str,
        witness: &Value,
    ) -> Result<Option<ManagerActionReceiptV2>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        match self.settle_uncertain_manager_action_on(
            operation.receipt.operation_id,
            operation.receipt.row_version,
            state,
            outcome,
            witness,
            None,
            None,
        ) {
            Ok(receipt) => {
                tx.commit()?;
                Ok(Some(receipt))
            }
            Err(error)
                if error.to_string().contains("manager_v2_action_changed")
                    || error
                        .to_string()
                        .contains("manager_v2_action_not_uncertain") =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Manager-requested settlement. The runtime gate rechecks the claim,
    /// scope, policy, capability and the exact uncertain row version.
    pub(crate) fn apply_manager_settle_action(
        &self,
        claim: &ManagerActionClaimV2,
        state: ManagerActionStateV2,
        outcome: &str,
        witness: &Value,
    ) -> Result<ManagerActionReceiptV2> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let ManagerActionV2::SettleUncertainAction {
            operation_id,
            expected_row_version,
        } = claim.action().clone()
        else {
            return Err(refused("manager_v2_not_settle_action"));
        };
        let settled = self.settle_uncertain_manager_action_on(
            operation_id,
            expected_row_version,
            state,
            outcome,
            witness,
            Some(claim.id()),
            Some(authority.caller),
        )?;
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            if settled.state == ManagerActionStateV2::Succeeded {
                "uncertain_action_settled_succeeded"
            } else {
                "uncertain_action_settled_failed"
            },
        )?;
        tx.commit()?;
        Ok(settled)
    }

    /// CAS `uncertain@row_version -> succeeded|failed` in the original scope
    /// and append `action_result` plus `action_settled` evidence events.
    #[allow(clippy::too_many_arguments)]
    fn settle_uncertain_manager_action_on(
        &self,
        operation_id: Uuid,
        expected_row_version: i64,
        state: ManagerActionStateV2,
        outcome: &str,
        witness: &Value,
        settled_by: Option<Uuid>,
        actor: Option<Uuid>,
    ) -> Result<ManagerActionReceiptV2> {
        if !matches!(
            state,
            ManagerActionStateV2::Succeeded | ManagerActionStateV2::Failed
        ) {
            return Err(refused("manager_v2_invalid_result"));
        }
        text(outcome, 256).map_err(refused)?;
        let op = self
            .manager_action_operation(operation_id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if op.receipt.state != ManagerActionStateV2::Uncertain {
            return Err(refused("manager_v2_action_not_uncertain"));
        }
        if op.receipt.row_version != expected_row_version {
            return Err(refused("manager_v2_action_changed"));
        }
        let prior = op.receipt.clone();
        let mut receipt = op.receipt;
        receipt.state = state;
        receipt.row_version += 1;
        receipt.outcome = Some(outcome.into());
        receipt.deduplicated = false;
        receipt.refresh_action_metadata(&op.context.request.operation);
        let changed = self.conn.execute(
            "UPDATE harness_manager_v2_operations SET state=?2,row_version=?3,outcome_json=?4,updated_at=?5
             WHERE id=?1 AND kind=?6 AND state='uncertain' AND row_version=?7",
            params![
                operation_id.to_string(),
                state_name(state),
                receipt.row_version,
                serde_json::to_string(&receipt)?,
                now(),
                ACTION_KIND,
                expected_row_version
            ],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_action_changed"));
        }
        let config = HarnessManagerConfigV1 {
            project_id: op.project_id,
            manager_session_id: op.manager_session_id,
            current_session_id: None,
            scope_mode: HarnessManagerScopeModeV1::Selected,
            selected_epic_ids: None,
            group_ids: Vec::new(),
            epic_ids: Vec::new(),
            row_version: op.scope_version,
            updated_at: Utc::now(),
        };
        let original_actor = match op.context.origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        let key = operation_id.to_string();
        self.manager_v2_event(
            &config,
            original_actor,
            "action_result",
            &key,
            receipt.row_version,
            &serde_json::to_value(&receipt)?,
        )?;
        self.manager_v2_event(
            &config,
            actor,
            "action_settled",
            &key,
            receipt.row_version,
            &json!({
                "operation_id": operation_id,
                "settled_state": state,
                "outcome": outcome,
                "prior_receipt": prior,
                "settled_by_operation_id": settled_by,
                "witness": witness,
            }),
        )?;
        Ok(receipt)
    }

    /// One transaction: recheck the fence and operator gates, disable (never
    /// delete) resume wakes and the sentinel slot, record the exact witness,
    /// audit and finish. The caller holds the lead spawn guard and has proved
    /// the lead absent from the active map.
    pub(crate) fn apply_manager_retire_continuations(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let ManagerActionV2::RetireLeadContinuations { epic_id, expected } = claim.action() else {
            return Err(refused("manager_v2_not_retire_action"));
        };
        let lead = expected
            .lead_session_id
            .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
        let superseded = crate::session::lifecycle::classify_manager_program_evidence(self, lead)?;
        let sentinel =
            crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(lead);
        let disabled: Vec<String> = {
            let mut statement = self.conn.prepare(
                "SELECT id FROM scheduled_jobs j WHERE enabled=1
                   AND ((wake_session_id=?1 AND (wake_mode='resume' OR (wake_mode LIKE 'on_terminal:%'
                         AND NOT EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=j.id))))
                        OR id=?2) ORDER BY id",
            )?;
            statement
                .query_map(params![lead.to_string(), sentinel.to_string()], |r| {
                    r.get(0)
                })?
                .collect::<std::result::Result<_, _>>()?
        };
        let stamp = now();
        for id in &disabled {
            self.conn.execute(
                "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                params![id, stamp],
            )?;
        }
        let event_sequence: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
            [lead.to_string()],
            |r| r.get(0),
        )?;
        // `MAX(sequence)` is 0 both for "no events" and "one event at
        // sequence 0"; the count tells them apart (design test 10).
        let event_count: i64 = self.conn.query_row(
            "SELECT count(*) FROM conversation_events WHERE session_id=?1",
            [lead.to_string()],
            |r| r.get(0),
        )?;
        let last_user_sequence: Option<i64> = self.conn.query_row(
            "SELECT MAX(sequence) FROM conversation_events WHERE session_id=?1 AND role='User'",
            [lead.to_string()],
            |r| r.get(0),
        )?;
        let witness = json!({
            "operation_id": claim.id(),
            "epic_id": epic_id,
            "lead_session_id": lead,
            "lead_generation": expected.lead_generation,
            "event_sequence": event_sequence,
            "event_count": event_count,
            "last_user_sequence": last_user_sequence,
            "superseded": superseded,
            "disabled_job_ids": disabled,
            "retired_at": stamp,
        });
        let key = lead.to_string();
        let prior = self.manager_v2_record(&authority.config, LEAD_RETIREMENT_KIND, &key)?;
        self.manager_v2_put_record(
            &authority.config,
            LEAD_RETIREMENT_KIND,
            &key,
            Some(*epic_id),
            prior.map_or(0, |r| r.row_version),
            &witness,
        )?;
        let actor = match claim.operation.context.origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.manager_v2_event(
            &authority.config,
            actor,
            "lead_continuations_retired",
            &claim.id().to_string(),
            1,
            &witness,
        )?;
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "lead_continuations_retired",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// True only while the lead's evidence is exactly what a manager
    /// retirement witnessed: no conversation event since, and the sentinel
    /// slot not re-registered. Any later output restores normal evaluation.
    /// A witness carrying `event_count` also requires the same event count,
    /// so the first event of a lead retired with no events stales it; an
    /// older witness without the field keeps its sequence-only meaning.
    pub(crate) fn manager_lead_program_outcome_superseded(&self, target: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_records r JOIN sessions s ON s.id=?1 AND r.project_id=s.project_id
                 WHERE r.kind=?2 AND r.record_key=?1 AND r.archived=0
                   AND json_extract(r.payload_json,'$.lead_session_id')=?1
                   AND json_extract(r.payload_json,'$.event_sequence')=
                       (SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1)
                   AND (json_extract(r.payload_json,'$.event_count') IS NULL
                        OR json_extract(r.payload_json,'$.event_count')=
                           (SELECT count(*) FROM conversation_events WHERE session_id=?1)))
               AND NOT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id=?3 AND enabled=1)",
            params![
                target.to_string(),
                LEAD_RETIREMENT_KIND,
                crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(
                    target
                )
                .to_string()
            ],
            |r| r.get(0),
        )?)
    }
}
