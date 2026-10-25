//! Withdrawal of a create that admission has not claimed. The queue claim and
//! withdrawal both CAS the same row, so exactly one can win.

use super::*;

impl Store {
    pub(super) fn manager_withdraw_create_target(
        &self,
        authority: &ManagerAuthorityV2,
        operation_id: Uuid,
        expected_row_version: i64,
        check_version: bool,
    ) -> Result<ManagerActionOperationV2> {
        let op = self
            .manager_action_operation(operation_id)?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        if op.project_id != authority.config.project_id {
            return Err(refused("manager_v2_action_out_of_scope"));
        }
        self.manager_settle_scope(authority, &op, 0)?;
        if !matches!(
            op.context.request.operation,
            ManagerActionV2::CreateSession { .. }
        ) {
            return Err(refused("manager_v2_action_not_queued_create"));
        }
        if check_version {
            if op.effect_started
                || op.receipt.state == ManagerActionStateV2::Running
                || op
                    .receipt
                    .target_session_id
                    .map(|id| self.get_session(id))
                    .transpose()?
                    .flatten()
                    .is_some()
            {
                return Err(refused(
                    "manager_v2_action_started_use_agent_halt: use AgentHalt on target_session_id",
                ));
            }
            if op.receipt.state != ManagerActionStateV2::Queued {
                return Err(refused("manager_v2_action_not_queued_create"));
            }
            if op.receipt.row_version != expected_row_version {
                return Err(refused("manager_v2_action_changed"));
            }
        }
        Ok(op)
    }

    pub fn apply_manager_withdraw_create(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_runtime_gate_on(claim)?;
        let ManagerActionV2::WithdrawQueuedCreate {
            operation_id,
            expected_row_version,
        } = claim.action()
        else {
            return Err(refused("manager_v2_action_not_queued_create"));
        };
        let op = self.manager_withdraw_create_target(
            &authority,
            *operation_id,
            *expected_row_version,
            true,
        )?;
        let mut receipt = op.receipt.clone();
        receipt.state = ManagerActionStateV2::Revoked;
        receipt.row_version += 1;
        receipt.outcome = Some("manager_v2_queued_create_withdrawn".into());
        receipt.deduplicated = false;
        receipt.refresh_action_metadata(&op.context.request.operation);
        let changed = self.conn.execute(
            "UPDATE harness_manager_v2_operations SET state='revoked',row_version=?2,outcome_json=?3,updated_at=?4
             WHERE id=?1 AND kind=?5 AND state='queued' AND row_version=?6 AND claim_boot_id IS NULL",
            params![operation_id.to_string(), receipt.row_version, serde_json::to_string(&receipt)?, now(), ACTION_KIND, expected_row_version],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_action_changed"));
        }
        self.conn.execute(
            "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at)
             VALUES(?1,?2,?3,?4,'action_result',?5,?6,?7,?8)",
            params![op.project_id.to_string(), op.manager_session_id.to_string(), op.scope_version, authority.caller.to_string(), operation_id.to_string(), receipt.row_version, serde_json::to_string(&receipt)?, now()],
        )?;
        self.conn.execute(
            "UPDATE harness_manager_v2_records SET archived=1,updated_at=?5
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND record_key=?4 AND kind IN ('lifecycle_context','lifecycle_execution')",
            params![
                op.project_id.to_string(),
                op.manager_session_id.to_string(),
                op.scope_version,
                operation_id.to_string(),
                now()
            ],
        )?;
        self.retire_unstarted_issue_worker_watch_on(&op)?;
        self.finish_manager_action_on(
            claim,
            ManagerActionStateV2::Succeeded,
            "manager_v2_queued_create_withdrawn",
        )?;
        tx.commit()?;
        if let Err(error) = self.note_issue_worker_launch_ended(
            &op,
            receipt.state,
            "manager_v2_queued_create_withdrawn",
        ) {
            tracing::warn!(%operation_id, %error, "withdrawn issue worker launch note not appended");
        }
        Ok(())
    }
}
