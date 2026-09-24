//! Operator-only exact decision ingress and durable answer delivery.

use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::*;
use rusqlite::{Transaction, TransactionBehavior};
use serde_json::json;
use uuid::Uuid;

use super::Store;
use super::harness_manager_v2::{fingerprint, refused};
use super::manager_coordinator::ManagerDecisionDeliveryV2;
use crate::error::Result;

impl Store {
    /// The RPC is operator-only. The callback is the ledger's exact work-gate
    /// admission, called within this same transaction only for the literal
    /// `accept` answer to a daemon-created acceptance decision.
    pub(crate) fn manager_v2_prepare_decision_answer(
        &self,
        request: &AnswerHarnessManagerDecisionRequestV2,
        apply_acceptance: impl FnOnce(
            &HarnessManagerConfigV1,
            &str,
            i64,
            &str,
        ) -> Result<ManagerMutationReceiptV2>,
    ) -> Result<ManagerMutationReceiptV2> {
        request.fence.validate().map_err(refused)?;
        text(&request.idempotency_key, 128).map_err(refused)?;
        text(&request.decision_key, 256).map_err(refused)?;
        text(&request.answer, 16384).map_err(refused)?;
        if request.expected_row_version <= 0 {
            return Err(refused("manager_v2_decision_changed"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let config = self
            .get_harness_manager(request.project_id)?
            .ok_or_else(|| refused("manager_not_configured"))?;
        let grant = self
            .get_harness_manager_policy(request.project_id)?
            .ok_or_else(|| refused("manager_v2_explicit_grant_required"))?;
        if grant.revoked
            || config.row_version != request.fence.scope_version
            || grant.row_version != request.fence.policy_version
        {
            return Err(refused("manager_v2_decision_scope_changed"));
        }
        let record = self
            .manager_v2_record(&config, "decision", &request.decision_key)?
            .ok_or_else(|| refused("manager_v2_decision_missing"))?;
        let epic = record
            .epic_id
            .ok_or_else(|| refused("manager_v2_decision_target_required"))?;
        if !config.epic_ids.contains(&epic) {
            return Err(refused("manager_v2_scope_denied"));
        }
        self.manager_epic(config.project_id, epic)?;
        let payload = serde_json::to_value(request)?;
        if let Some(replay) = self.manager_v2_replay(&config, &request.idempotency_key, &payload)? {
            tx.commit()?;
            return Ok(serde_json::from_value(replay)?);
        }
        if record.row_version != request.expected_row_version
            || record.payload["target_digest"] != request.target_digest
            || record.payload["status"] != "pending"
        {
            return Err(refused("manager_v2_decision_changed"));
        }
        let mut decision = record.payload;
        let mut queued_delivery = None;
        let is_acceptance =
            request.decision_key.starts_with("accept:") && decision["target_row_version"].is_i64();
        if is_acceptance && request.answer.trim() == "accept" {
            apply_acceptance(
                &config,
                &request.decision_key,
                record.row_version,
                &request.target_digest,
            )?;
            decision["status"] = json!("answered");
            decision["answer"] = json!(request.answer);
            decision["delivery"] = json!({"state":"applied","kind":"exact_work_acceptance"});
        } else if is_acceptance {
            if request.answer.trim() != "decline" {
                return Err(refused(
                    "manager_v2_acceptance_answer_must_be_accept_or_decline",
                ));
            }
            decision["status"] = json!("declined");
            decision["answer"] = json!(request.answer);
            decision["delivery"] = json!({"state":"applied","kind":"acceptance_declined"});
        } else if let Some(target) =
            self.manager_v2_record(&config, "decision_target", &request.decision_key)?
        {
            if fingerprint(&target.payload)? != request.target_digest {
                return Err(refused("manager_v2_decision_target_changed"));
            }
            let delivery_id = Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!(
                    "manager-answer:{}:{}:{}:{}",
                    config.project_id,
                    config.manager_session_id,
                    config.row_version,
                    request.idempotency_key
                )
                .as_bytes(),
            );
            let delivery = ManagerDecisionDeliveryV2 {
                project_id: config.project_id,
                manager_session_id: config.manager_session_id,
                scope_version: config.row_version,
                policy_version: grant.row_version,
                key: delivery_id.to_string(),
                decision_key: request.decision_key.clone(),
                epic_id: Some(epic),
                target_digest: request.target_digest.clone(),
                target: target.payload,
                answer: request.answer.clone(),
                state: "queued".into(),
                effect_started: false,
                boot_id: None,
                outcome: None,
            };
            self.manager_v2_put_record(
                &config,
                "decision_delivery",
                &delivery.key,
                Some(epic),
                0,
                &serde_json::to_value(&delivery)?,
            )?;
            decision["status"] = json!("answer_queued");
            decision["answer"] = json!(request.answer);
            decision["delivery"] = json!({"state":"queued","delivery_id":delivery_id});
            queued_delivery = Some(delivery);
        } else {
            // Manager questions are delivered as attributed OPERATOR answers
            // in the scoped decision inbox. No v1 agent sender is fabricated.
            // Retrieval does not mark the associated work/request completed.
            if let Some(work_key) = decision["work_key"].as_str() {
                let work = self
                    .manager_v2_record(&config, "work", work_key)?
                    .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
                if work.epic_id != Some(epic)
                    || Some(work.row_version) != decision["target_row_version"].as_i64()
                {
                    return Err(refused("manager_v2_decision_target_changed"));
                }
            }
            if let Some(request_id) = decision["request_id"].as_str() {
                let id = Uuid::parse_str(request_id)
                    .map_err(|_| refused("manager_v2_request_changed"))?;
                if !self.manager_v2_operator_request_live(&config, epic, id)? {
                    return Err(refused("manager_v2_request_changed"));
                }
                let version = self
                    .manager_v2_record(&config, "request", request_id)?
                    .map_or(0, |r| r.row_version);
                if Some(version) != decision["request_row_version"].as_i64() {
                    return Err(refused("manager_v2_request_changed"));
                }
            }
            decision["status"] = json!("answered");
            decision["answer"] = json!(request.answer);
            decision["delivery"] = json!({"state":"available_in_scoped_inbox","actor":"operator","request_id":decision["request_id"],"work_key":decision["work_key"],"target_digest":request.target_digest});
        }
        let updated = self.manager_v2_put_record(
            &config,
            "decision",
            &request.decision_key,
            Some(epic),
            record.row_version,
            &decision,
        )?;
        if let Some(delivery) = &queued_delivery {
            self.manager_v2_check_decision_target(delivery)?;
        }
        let event = self.manager_v2_event(
            &config,
            None,
            "operator_decision_answer",
            &request.decision_key,
            updated.row_version,
            &decision,
        )?;
        let receipt = ManagerMutationReceiptV2 {
            event_sequence: event,
            key: request.decision_key.clone(),
            row_version: updated.row_version,
            deduplicated: false,
        };
        self.manager_v2_save_receipt(
            &config,
            None,
            grant.row_version,
            "operator_decision_answer",
            &request.idempotency_key,
            &payload,
            &serde_json::to_value(&receipt)?,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn manager_v2_check_decision_target(
        &self,
        delivery: &ManagerDecisionDeliveryV2,
    ) -> Result<HarnessManagerConfigV1> {
        let config = self
            .get_harness_manager(delivery.project_id)?
            .ok_or_else(|| refused("manager_v2_decision_scope_changed"))?;
        let grant = self
            .get_harness_manager_policy(delivery.project_id)?
            .ok_or_else(|| refused("manager_v2_decision_scope_changed"))?;
        if grant.revoked
            || config.manager_session_id != delivery.manager_session_id
            || config.row_version != delivery.scope_version
            || grant.row_version != delivery.policy_version
        {
            return Err(refused("manager_v2_decision_scope_changed"));
        }
        let session_id = delivery.target["session_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| refused("manager_v2_decision_target_required"))?;
        let epic = self.manager_v2_live_epic_for_session(&config, session_id)?;
        // This record captures the owning Epic at question publication. Neither
        // historical resource attribution nor a reparent into another selected
        // Epic can redirect an already approved answer.
        let target = self
            .manager_v2_record(&config, "decision_target", &delivery.decision_key)?
            .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
        if delivery.epic_id != Some(epic)
            || target.epic_id != Some(epic)
            || target.payload != delivery.target
        {
            return Err(refused("manager_v2_decision_target_changed"));
        }
        let decision = self
            .manager_v2_record(&config, "decision", &delivery.decision_key)?
            .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
        if decision.epic_id != delivery.epic_id
            || decision.payload["target_digest"] != delivery.target_digest
            || decision.payload["status"] != "answer_queued"
            || decision.payload["delivery"]["delivery_id"] != delivery.key
        {
            return Err(refused("manager_v2_decision_target_changed"));
        }
        match delivery.target["kind"].as_str() {
            Some("question") => {
                let actual = self
                    .manager_v2_question_target(session_id)?
                    .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
                if actual != delivery.target || fingerprint(&actual)? != delivery.target_digest {
                    return Err(refused("manager_v2_decision_target_changed"));
                }
            }
            Some("appserver_approval") => {
                if !matches!(delivery.answer.trim(), "approve" | "deny") {
                    return Err(refused(
                        "manager_v2_approval_answer_must_be_approve_or_deny",
                    ));
                }
                crate::codex_app_server::approval_response(
                    &delivery.target["request_id"],
                    delivery.target["method"].as_str().unwrap_or_default(),
                    &delivery.target["params"],
                    if delivery.answer.trim() == "approve" {
                        crate::provider::ApprovalDecision::Approve
                    } else {
                        crate::provider::ApprovalDecision::Deny
                    },
                )?;
                let actual = self
                    .appserver_approval_target(
                        session_id,
                        delivery.target["publication_id"]
                            .as_str()
                            .unwrap_or_default(),
                    )?
                    .ok_or_else(|| refused("manager_v2_decision_target_changed"))?;
                if actual != delivery.target || fingerprint(&actual)? != delivery.target_digest {
                    return Err(refused("manager_v2_decision_target_changed"));
                }
            }
            _ => return Err(refused("manager_v2_decision_route_unavailable")),
        }
        Ok(config)
    }

    /// Update the journal, exact public decision and immutable event together.
    /// This internal journal scope grants no new effect authority; old receipts
    /// must still settle after an appointment is revoked.
    pub(super) fn manager_v2_delivery_transition_on(
        &self,
        config: &HarnessManagerConfigV1,
        record: &super::harness_manager_v2::ManagerRecordV2,
        delivery: &ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        self.manager_v2_put_record(
            config,
            "decision_delivery",
            &record.key,
            record.epic_id,
            record.row_version,
            &serde_json::to_value(delivery)?,
        )?;
        if let Some(decision) =
            self.manager_v2_record(config, "decision", &delivery.decision_key)?
        {
            if decision.epic_id == delivery.epic_id
                && decision.payload["target_digest"] == delivery.target_digest
                && decision.payload["delivery"]["delivery_id"] == delivery.key
            {
                let mut payload = decision.payload;
                payload["delivery"] = json!({"state":delivery.state,"delivery_id":delivery.key,"outcome":delivery.outcome});
                let approval_unavailable = delivery.target["kind"] == "appserver_approval"
                    && delivery.target["session_id"]
                        .as_str()
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .is_none_or(|id| {
                            !self
                                .appserver_approval_target(
                                    id,
                                    delivery.target["publication_id"]
                                        .as_str()
                                        .unwrap_or_default(),
                                )
                                .is_ok_and(|actual| actual.as_ref() == Some(&delivery.target))
                        });
                payload["status"] = if delivery.target["kind"] == "appserver_approval"
                    && payload["closure_state"] == "closed"
                {
                    json!("resolved")
                } else if delivery.target["kind"] == "appserver_approval"
                    && payload["status"] == "superseded"
                {
                    json!("superseded")
                } else {
                    json!(match delivery.state.as_str() {
                        "delivered" => "answered",
                        "enqueued" => "answer_sent",
                        "blocked" | "revoked" if approval_unavailable => "blocked",
                        "blocked" | "revoked" if !delivery.effect_started => "pending",
                        _ => "answer_queued",
                    })
                };
                self.manager_v2_put_record(
                    config,
                    "decision",
                    &delivery.decision_key,
                    decision.epic_id,
                    decision.row_version,
                    &payload,
                )?;
            }
        }
        self.manager_v2_event(
            config,
            None,
            "decision_delivery",
            &delivery.key,
            record.row_version + 1,
            &serde_json::to_value(delivery)?,
        )?;
        Ok(())
    }

    /// Before-effect claims are safe to resume; effect-marked claims retain
    /// visible uncertainty in both projections and are never automatically resent.
    pub(crate) fn manager_v2_recover_decision_deliveries(&self, boot: Uuid) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut stmt=self.conn.prepare("SELECT payload_json FROM harness_manager_v2_records INDEXED BY harness_manager_v2_running_answers WHERE kind='decision_delivery' AND json_extract(payload_json,'$.state')='running' AND json_extract(payload_json,'$.boot_id') IS NOT ?1 ORDER BY project_id,record_key LIMIT 32")?;
        let rows = stmt
            .query_map([boot.to_string()], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for raw in &rows {
            let mut delivery: ManagerDecisionDeliveryV2 = serde_json::from_str(raw)?;
            let config = delivery_journal_scope(&delivery);
            let record = self
                .manager_v2_record(&config, "decision_delivery", &delivery.key)?
                .ok_or_else(|| refused("manager_v2_decision_delivery_missing"))?;
            if record.payload != serde_json::to_value(&delivery)? {
                return Err(refused("manager_v2_decision_claim_changed"));
            }
            delivery.state = if delivery.effect_started {
                "uncertain"
            } else {
                "queued"
            }
            .into();
            delivery.outcome=Some(if delivery.effect_started {"provider answer effect may have occurred before daemon restart; inspect the exact target before any operator-directed recovery"} else {"recovered answer before provider effect"}.into());
            self.manager_v2_delivery_transition_on(&config, &record, &delivery)?;
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub(crate) fn manager_v2_claim_decision_delivery(
        &self,
        config: &HarnessManagerConfigV1,
        boot: Uuid,
    ) -> Result<Option<ManagerDecisionDeliveryV2>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for record in self.manager_v2_queued_decision_deliveries(config)? {
            let mut delivery: ManagerDecisionDeliveryV2 =
                serde_json::from_value(record.payload.clone())?;
            if delivery.state != "queued" {
                continue;
            }
            if let Err(error) = self.manager_v2_check_decision_target(&delivery) {
                delivery.state = "revoked".into();
                delivery.outcome = Some(error.to_string());
                self.manager_v2_delivery_transition_on(config, &record, &delivery)?;
                continue;
            }
            delivery.state = "running".into();
            delivery.boot_id = Some(boot);
            self.manager_v2_delivery_transition_on(config, &record, &delivery)?;
            tx.commit()?;
            return Ok(Some(delivery));
        }
        tx.commit()?;
        Ok(None)
    }

    pub(crate) fn manager_v2_set_decision_delivery(
        &self,
        expected: &ManagerDecisionDeliveryV2,
        state: &str,
        effect_started: bool,
        outcome: Option<String>,
    ) -> Result<ManagerDecisionDeliveryV2> {
        if !matches!(
            state,
            "running" | "delivered" | "blocked" | "uncertain" | "revoked"
        ) || (expected.effect_started && (!effect_started || state == "running"))
        {
            return Err(refused("manager_v2_invalid_delivery_state"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let config = delivery_journal_scope(expected);
        let record = self
            .manager_v2_record(&config, "decision_delivery", &expected.key)?
            .ok_or_else(|| refused("manager_v2_decision_delivery_missing"))?;
        if record.payload != serde_json::to_value(expected)? || expected.state != "running" {
            return Err(refused("manager_v2_decision_claim_changed"));
        }
        if state == "running" {
            self.manager_v2_check_decision_target(expected)?;
        }
        let mut delivery = expected.clone();
        delivery.state = state.into();
        delivery.effect_started = effect_started;
        delivery.outcome = outcome;
        self.manager_v2_delivery_transition_on(&config, &record, &delivery)?;
        tx.commit()?;
        Ok(delivery)
    }
}

// Identity-only view for settling an existing journal row, never passed to an
// authorization method or used to authorize another provider effect.
fn delivery_journal_scope(delivery: &ManagerDecisionDeliveryV2) -> HarnessManagerConfigV1 {
    HarnessManagerConfigV1 {
        scope_mode: rsi_common::harness_manager::HarnessManagerScopeModeV1::Selected,
        selected_epic_ids: None,
        group_ids: Vec::new(),
        project_id: delivery.project_id,
        manager_session_id: delivery.manager_session_id,
        current_session_id: None,
        epic_ids: vec![],
        row_version: delivery.scope_version,
        updated_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_coordinator::tests::{fixture, raise_question};

    fn request(
        store: &Store,
        config: &HarnessManagerConfigV1,
        session: Uuid,
    ) -> AnswerHarnessManagerDecisionRequestV2 {
        store.manager_v2_refresh_question_decisions(config).unwrap();
        let key = format!("question:{session}");
        let decision = store
            .manager_v2_record(config, "decision", &key)
            .unwrap()
            .unwrap();
        AnswerHarnessManagerDecisionRequestV2 {
            project_id: config.project_id,
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: 1,
            },
            decision_key: key,
            expected_row_version: decision.row_version,
            target_digest: decision.payload["target_digest"].as_str().unwrap().into(),
            answer: "Use the reserved version".into(),
            idempotency_key: "answer-1".into(),
        }
    }

    #[test]
    fn manager_v2_decision_answer_is_exact_replayable_and_keeps_question_until_delivery() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        let request = request(&store, &config, lead.id);
        let receipt = store
            .manager_v2_prepare_decision_answer(&request, |_, _, _, _| {
                panic!("question is not acceptance")
            })
            .unwrap();
        assert!(!receipt.deduplicated);
        assert!(
            store
                .get_session(lead.id)
                .unwrap()
                .unwrap()
                .pending_question
                .is_some()
        );
        let replay = store
            .manager_v2_prepare_decision_answer(&request, |_, _, _, _| {
                panic!("replay is not acceptance")
            })
            .unwrap();
        assert!(replay.deduplicated);
        assert_eq!(replay.event_sequence, receipt.event_sequence);
        let deliveries = store
            .manager_v2_records_of_kind(&config, "decision_delivery")
            .unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].payload["state"], "queued");
        let mut changed = request;
        changed.answer = "different answer".into();
        assert!(
            store
                .manager_v2_prepare_decision_answer(&changed, |_, _, _, _| unreachable!())
                .is_err()
        );
    }

    #[test]
    fn manager_v2_decision_old_answer_cannot_clear_new_same_text_question() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        let old = request(&store, &config, lead.id);
        raise_question(&store, lead.id, 2);
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap();
        assert!(
            store
                .manager_v2_prepare_decision_answer(&old, |_, _, _, _| unreachable!())
                .unwrap_err()
                .to_string()
                .contains("decision_changed")
        );
        let target = store.manager_v2_question_target(lead.id).unwrap().unwrap();
        assert_eq!(target["event_sequence"], 2);
        assert!(
            store
                .get_session(lead.id)
                .unwrap()
                .unwrap()
                .pending_question
                .is_some()
        );
    }

    #[test]
    fn manager_v2_decision_restart_recovers_pre_effect_and_preserves_post_effect_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decision.db");
        let store = Store::open(&path).unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        let request = request(&store, &config, lead.id);
        store
            .manager_v2_prepare_decision_answer(&request, |_, _, _, _| unreachable!())
            .unwrap();
        let boot1 = Uuid::new_v4();
        store
            .manager_v2_claim_decision_delivery(&config, boot1)
            .unwrap()
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let boot2 = Uuid::new_v4();
        assert_eq!(
            store.manager_v2_recover_decision_deliveries(boot2).unwrap(),
            1
        );
        let recovered = store
            .manager_v2_claim_decision_delivery(&config, boot2)
            .unwrap()
            .unwrap();
        let marked = store
            .manager_v2_set_decision_delivery(&recovered, "running", true, None)
            .unwrap();
        assert!(marked.effect_started);
        drop(store);
        let store = Store::open(&path).unwrap();
        let boot3 = Uuid::new_v4();
        assert_eq!(
            store.manager_v2_recover_decision_deliveries(boot3).unwrap(),
            1
        );
        let rows = store
            .manager_v2_records_of_kind(&config, "decision_delivery")
            .unwrap();
        assert_eq!(rows[0].payload["state"], "uncertain");
        let decision = store
            .manager_v2_record(&config, "decision", &request.decision_key)
            .unwrap()
            .unwrap();
        assert_eq!(decision.payload["delivery"]["state"], "uncertain");
        assert_eq!(decision.payload["status"], "answer_queued");
        let inbox = store
            .manager_v2_inspect_operator(
                config.project_id,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Decisions,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(inbox.rows[0]["delivery"]["state"], "uncertain");
        assert!(
            store
                .manager_v2_claim_decision_delivery(&config, boot3)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_session(lead.id)
                .unwrap()
                .unwrap()
                .pending_question
                .is_some()
        );
    }

    #[test]
    fn manager_v2_decision_policy_revocation_prevents_queued_delivery() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        let request = request(&store, &config, lead.id);
        store
            .manager_v2_prepare_decision_answer(&request, |_, _, _, _| unreachable!())
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: config.project_id,
                expected_scope_version: config.row_version,
                expected_policy_version: 1,
                idempotency_key: "revised-policy".into(),
                policy: ManagerPolicyV2 {
                    paused: true,
                    ..Default::default()
                },
            })
            .unwrap();
        assert!(
            store
                .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .manager_v2_records_of_kind(&config, "decision_delivery")
                .unwrap()[0]
                .payload["state"],
            "revoked"
        );
        let decision = store
            .manager_v2_record(&config, "decision", &request.decision_key)
            .unwrap()
            .unwrap();
        assert_eq!(decision.payload["delivery"]["state"], "revoked");
        assert_eq!(decision.payload["status"], "pending");
        assert!(
            store
                .get_session(lead.id)
                .unwrap()
                .unwrap()
                .pending_question
                .is_some()
        );
    }

    #[test]
    fn manager_v2_decision_terminal_settlement_survives_appointment_revocation() {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        let request = request(&store, &config, lead.id);
        store
            .manager_v2_prepare_decision_answer(&request, |_, _, _, _| unreachable!())
            .unwrap();
        let claimed = store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .unwrap();
        let marked = store
            .manager_v2_set_decision_delivery(&claimed, "running", true, None)
            .unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: config.project_id,
                session_id: config.manager_session_id,
                epic_ids: Some(vec![]),
                expected_row_version: config.row_version,
            })
            .unwrap();
        let delivered = store
            .manager_v2_set_decision_delivery(
                &marked,
                "delivered",
                true,
                Some("provider already established".into()),
            )
            .unwrap();
        assert_eq!(delivered.state, "delivered");
        assert_eq!(
            store
                .manager_v2_record(&config, "decision", &request.decision_key)
                .unwrap()
                .unwrap()
                .payload["delivery"]["state"],
            "delivered"
        );
    }

    #[test]
    fn manager_v2_decision_before_effect_failure_allows_new_exact_operator_attempt() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        let first = request(&store, &config, lead.id);
        store
            .manager_v2_prepare_decision_answer(&first, |_, _, _, _| unreachable!())
            .unwrap();
        let claimed = store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .unwrap();
        store
            .manager_v2_set_decision_delivery(
                &claimed,
                "blocked",
                false,
                Some("provider capacity unavailable".into()),
            )
            .unwrap();
        let mut retry = request(&store, &config, lead.id);
        retry.idempotency_key = "operator-answer-2".into();
        let receipt = store
            .manager_v2_prepare_decision_answer(&retry, |_, _, _, _| unreachable!())
            .unwrap();
        assert!(receipt.row_version > first.expected_row_version);
        let next = store
            .manager_v2_claim_decision_delivery(&config, Uuid::new_v4())
            .unwrap()
            .unwrap();
        assert_ne!(next.key, claimed.key);
        assert_eq!(next.target_digest, claimed.target_digest);
    }
}
