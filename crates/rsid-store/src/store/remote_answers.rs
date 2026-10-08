//! Durable remote-answer ingress and claim fences. No provider effects live here.
use crate::error::{DaemonError, Result};
use crate::store::{
    Store,
    harness_manager_v2::{fingerprint, now, refused},
    pending_approvals::approval_decision_key,
};
use rsi_common::remote_pending_decisions::{
    AnswerPendingDecisionV1, RemoteAnswerReceiptV1, RemoteAnswerStateV1,
};
use rsi_common::remote_read::WireUuid;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAnswerDelivery {
    pub request: AnswerPendingDecisionV1,
    pub target: Value,
    pub receipt: RemoteAnswerReceiptV1,
    pub effect_started: bool,
    pub claim_boot_id: Option<Uuid>,
}

fn uuid(value: &WireUuid) -> Uuid {
    Uuid::parse_str(value.as_str()).expect("validated wire UUID")
}

fn request_fingerprint(request: &AnswerPendingDecisionV1) -> Result<String> {
    let mut value = serde_json::to_value(request)?;
    // The epoch records ingress provenance, not the phone's answer identity.
    value["origin"]
        .as_object_mut()
        .unwrap()
        .remove("gateway_epoch");
    fingerprint(&value)
}

impl RemoteAnswerDelivery {
    pub fn session_id(&self) -> Uuid {
        uuid(&self.request.session_id)
    }
    pub fn key(&self) -> &str {
        self.request.idempotency_key.as_str()
    }
}

impl Store {
    /// Identity comes only from the explicit producer publication, never from
    /// matching display text or the legacy pending snapshot.
    pub fn remote_pending_decision_target(
        &self,
        project: Uuid,
        session: Uuid,
        decision: &str,
    ) -> Result<Value> {
        let in_scope: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1 AND project_id=?2 AND status NOT IN ('Archived','Deleted'))",
            params![session.to_string(), project.to_string()], |r| r.get(0))?;
        if !in_scope {
            return Err(refused("decision_changed"));
        }
        let (kind, publication) = decision
            .split_once(':')
            .ok_or_else(|| refused("decision_changed"))?;
        if WireUuid::new(publication.to_owned()).is_err() {
            return Err(refused("decision_changed"));
        }
        let target = match kind {
            "pending-question" => self.pending_question_target(session),
            "pending-approval" => self.appserver_approval_target(session, publication),
            _ => return Err(refused("decision_changed")),
        }
        .map_err(|_| refused("decision_changed"))?;
        target
            .filter(|t| t["publication_id"] == publication)
            .ok_or_else(|| refused("decision_changed"))
    }

    /// Bounded producer-publication inventory for one project/session. Legacy
    /// snapshots and partial publications never become answer targets.
    pub fn remote_pending_decision_targets(
        &self,
        project: Uuid,
        session: Uuid,
        limit: usize,
    ) -> Result<Vec<(String, Value)>> {
        let mut ids = Vec::new();
        if let Ok(Some(target)) = self.pending_question_target(session) {
            ids.push(format!(
                "pending-question:{}",
                target["publication_id"].as_str().unwrap_or_default()
            ));
        }
        let approvals = {
            let mut stmt = self.conn.prepare("SELECT publication_id FROM appserver_approval_publications WHERE session_id=?1 AND state='published' AND closure_state='open' ORDER BY created_at,publication_id LIMIT ?2")?;
            stmt.query_map(params![session.to_string(), limit.min(17) as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ids.extend(
            approvals
                .into_iter()
                .map(|id| format!("pending-approval:{id}")),
        );
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                self.remote_pending_decision_target(project, session, &id)
                    .ok()
                    .map(|target| (id, target))
            })
            .take(limit.min(17))
            .collect())
    }

    pub fn remote_answer_receipt_for_target(
        &self,
        session: Uuid,
        decision: &str,
        digest: &str,
    ) -> Result<Option<RemoteAnswerReceiptV1>> {
        let raw: Option<String> = self.conn.query_row("SELECT outcome_json FROM remote_answer_deliveries WHERE session_id=?1 AND decision_id=?2 AND target_digest=?3", params![session.to_string(), decision, digest], |r| r.get(0)).optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .transpose()
    }

    pub fn remote_answer_delivery(&self, key: &str) -> Result<Option<RemoteAnswerDelivery>> {
        let row: Option<(String, String, String, bool, Option<String>)> = self.conn.query_row(
            "SELECT request_json,target_json,outcome_json,effect_started,claim_boot_id FROM remote_answer_deliveries WHERE idempotency_key=?1",
            [key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
        row.map(|(request, target, receipt, effect_started, boot)| {
            Ok(RemoteAnswerDelivery {
                request: serde_json::from_str(&request)?,
                target: serde_json::from_str(&target)?,
                receipt: serde_json::from_str(&receipt)?,
                effect_started,
                claim_boot_id: boot
                    .map(|v| {
                        Uuid::parse_str(&v).map_err(|_| refused("remote_answer_corrupt_claim"))
                    })
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Fingerprint replay precedes target reads, so an accepted receipt survives
    /// clearing/replacement of its question and changes to the project scope.
    pub fn prepare_remote_answer(
        &self,
        request: &AnswerPendingDecisionV1,
    ) -> Result<RemoteAnswerReceiptV1> {
        let raw = serde_json::to_string(request)?;
        if raw.len() > 4096
            || request.answer.as_str().contains('\0')
            || request.origin.client_node.as_str().contains('\0')
        {
            return Err(refused("remote_answer_invalid"));
        }
        let hash = request_fingerprint(request)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT request_fingerprint FROM remote_answer_deliveries WHERE idempotency_key=?1",
                [request.idempotency_key.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let delivery = self
                .remote_answer_delivery(request.idempotency_key.as_str())?
                .ok_or_else(|| refused("remote_answer_missing"))?;
            // V162 rows created before this change hashed the epoch too. Their
            // immutable saved request lets them replay without a migration.
            if old != hash && request_fingerprint(&delivery.request)? != hash {
                return Err(refused("idempotency_conflict"));
            }
            tx.commit()?;
            return Ok(delivery.receipt);
        }
        let target = self.remote_pending_decision_target(
            uuid(&request.project_id),
            uuid(&request.session_id),
            request.decision_id.as_str(),
        )?;
        if fingerprint(&target)? != request.expected_target_digest.as_str() {
            return Err(refused("decision_changed"));
        }
        let inbox_key = match target["kind"].as_str() {
            Some("question") => format!("question:{}", request.session_id.as_str()),
            Some("appserver_approval") => approval_decision_key(&target),
            _ => return Err(refused("decision_changed")),
        };
        if let Some(config) = self.get_harness_manager(uuid(&request.project_id))?
            && self
                .manager_v2_record(&config, "decision", &inbox_key)?
                .is_some_and(|row| {
                    row.payload["status"] == "pending"
                        && row.payload["target_digest"] == request.expected_target_digest.as_str()
                })
        {
            // Same exclusion as A1, after receipt replay so a later inbox
            // projection never hides an already accepted remote answer.
            return Err(refused("decision_changed"));
        }
        if target["kind"] == "appserver_approval"
            && !matches!(request.answer.as_str(), "approve" | "deny")
        {
            return Err(refused("remote_answer_approval_requires_approve_or_deny"));
        }
        let occupied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_answer_deliveries WHERE session_id=?1 AND decision_id=?2 AND target_digest=?3)",
            params![request.session_id.as_str(),request.decision_id.as_str(), request.expected_target_digest.as_str()], |r| r.get(0))?;
        if occupied {
            return Err(refused("decision_changed"));
        }
        let receipt = RemoteAnswerReceiptV1 {
            receipt_key: request.idempotency_key.clone(),
            state: RemoteAnswerStateV1::Queued,
            outcome: None,
        };
        let stamp = now();
        tx.execute("INSERT INTO remote_answer_deliveries(idempotency_key,request_fingerprint,request_json,project_id,session_id,decision_id,target_kind,target_json,target_digest,answer,origin_json,state,outcome_json,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'queued',?12,?13,?13)",
            params![request.idempotency_key.as_str(),hash,raw,request.project_id.as_str(),request.session_id.as_str(),request.decision_id.as_str(),target["kind"].as_str(),target.to_string(),request.expected_target_digest.as_str(),request.answer.as_str(),serde_json::to_string(&request.origin)?,serde_json::to_string(&receipt)?,stamp])?;
        tx.commit()?;
        Ok(receipt)
    }

    /// The exact durable claim and the real target are rechecked under the
    /// runtime spawn/writer guard immediately before marking provider intent.
    pub fn check_remote_answer_target(&self, delivery: &RemoteAnswerDelivery) -> Result<()> {
        let current = self
            .remote_answer_delivery(delivery.key())?
            .ok_or_else(|| refused("decision_changed"))?;
        if current.receipt.state != RemoteAnswerStateV1::Running
            || current.claim_boot_id != delivery.claim_boot_id
            || current.claim_boot_id.is_none()
            || current.request != delivery.request
            || current.target != delivery.target
            || current.effect_started != delivery.effect_started
        {
            return Err(refused("decision_changed"));
        }
        let target = self.remote_pending_decision_target(
            uuid(&delivery.request.project_id),
            delivery.session_id(),
            delivery.request.decision_id.as_str(),
        )?;
        if target != delivery.target
            || fingerprint(&target)? != delivery.request.expected_target_digest.as_str()
        {
            return Err(refused("decision_changed"));
        }
        Ok(())
    }

    pub fn claim_remote_answer(&self, boot: Uuid) -> Result<Option<RemoteAnswerDelivery>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let keys = {
            let mut stmt = tx.prepare("SELECT idempotency_key FROM remote_answer_deliveries WHERE state='queued' ORDER BY created_at,idempotency_key LIMIT 64")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for key in keys {
            let Some(mut delivery) = self.remote_answer_or_refuse_corrupt_on(&tx, &key)? else {
                continue;
            };
            delivery.receipt.state = RemoteAnswerStateV1::Running;
            tx.execute("UPDATE remote_answer_deliveries SET state='running',claim_boot_id=?1,outcome_json=?2,updated_at=?3 WHERE idempotency_key=?4 AND state='queued'", params![boot.to_string(),serde_json::to_string(&delivery.receipt)?,now(),key])?;
            delivery.claim_boot_id = Some(boot);
            tx.commit()?;
            return Ok(Some(delivery));
        }
        tx.commit()?;
        Ok(None)
    }

    /// Decode failures belong to one row, not the lane. Keep immutable ingress
    /// and effect intent for diagnosis, but settle the row so it is never sent.
    /// Database errors still abort the transaction rather than hiding a fault.
    fn remote_answer_or_refuse_corrupt_on(
        &self,
        tx: &Transaction<'_>,
        key: &str,
    ) -> Result<Option<RemoteAnswerDelivery>> {
        match self.remote_answer_delivery(key) {
            Ok(Some(delivery)) => Ok(Some(delivery)),
            Ok(None) => Err(refused("remote_answer_missing")),
            Err(error)
                if matches!(&error, DaemonError::Json(_))
                    || matches!(&error, DaemonError::InvalidParam(code) if code == "remote_answer_corrupt_claim") =>
            {
                let receipt = RemoteAnswerReceiptV1 {
                    receipt_key: WireUuid::new(key.to_owned())
                        .map_err(|_| refused("remote_answer_corrupt"))?,
                    state: RemoteAnswerStateV1::Refused,
                    outcome: Some(json!({"code":"remote_answer_corrupt"})),
                };
                tx.execute("UPDATE remote_answer_deliveries SET state='refused',claim_boot_id=NULL,outcome_json=?1,updated_at=?2 WHERE idempotency_key=?3 AND state IN ('queued','running')",
                    params![serde_json::to_string(&receipt)?,now(),key])?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn set_remote_answer_delivery(
        &self,
        delivery: &RemoteAnswerDelivery,
        state: RemoteAnswerStateV1,
        effect_started: bool,
        outcome: Option<Value>,
    ) -> Result<RemoteAnswerDelivery> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if state == RemoteAnswerStateV1::Running && effect_started {
            self.check_remote_answer_target(delivery)?;
        }
        let next =
            self.transition_remote_answer_on(&tx, delivery, state, effect_started, outcome)?;
        tx.commit()?;
        Ok(next)
    }

    fn transition_remote_answer_on(
        &self,
        tx: &Transaction<'_>,
        delivery: &RemoteAnswerDelivery,
        state: RemoteAnswerStateV1,
        effect_started: bool,
        outcome: Option<Value>,
    ) -> Result<RemoteAnswerDelivery> {
        let mut next = delivery.clone();
        next.receipt.state = state;
        next.receipt.outcome = outcome;
        next.effect_started = effect_started;
        let state = serde_json::to_value(state)?
            .as_str()
            .ok_or_else(|| refused("remote_answer_invalid_state"))?
            .to_owned();
        let changed = tx.execute("UPDATE remote_answer_deliveries SET state=?1,effect_started=?2,outcome_json=?3,updated_at=?4 WHERE idempotency_key=?5 AND state='running' AND claim_boot_id=?6 AND effect_started=?7",
            params![state,effect_started,serde_json::to_string(&next.receipt)?,now(),delivery.key(),delivery.claim_boot_id.map(|v|v.to_string()),delivery.effect_started])?;
        if changed != 1 {
            return Err(refused("decision_changed"));
        }
        Ok(next)
    }

    pub fn finish_remote_approval_enqueue(&self, delivery: &RemoteAnswerDelivery) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        self.check_remote_answer_target(delivery)?;
        if !delivery.effect_started || delivery.target["kind"] != "appserver_approval" {
            return Err(refused("decision_changed"));
        }
        self.mark_appserver_approval_enqueued(&delivery.target, delivery.request.answer.as_str())?;
        self.transition_remote_answer_on(&tx, delivery, RemoteAnswerStateV1::Succeeded, true,
            Some(json!({"code":"response_enqueued","detail":"provider consumption unconfirmed; never automatically resent"})))?;
        tx.commit()?;
        Ok(())
    }

    /// Restart retries only claims with no possible provider effect. Effect
    /// intent is durable before a write/spawn, and never reset after recovery.
    pub fn recover_remote_answers(&self, boot: Uuid) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let keys = {
            let mut stmt = tx.prepare("SELECT idempotency_key FROM remote_answer_deliveries WHERE state='running' AND claim_boot_id<>?1 ORDER BY created_at,idempotency_key LIMIT 64")?;
            stmt.query_map([boot.to_string()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for key in &keys {
            let Some(delivery) = self.remote_answer_or_refuse_corrupt_on(&tx, key)? else {
                continue;
            };
            let state = if delivery.effect_started {
                RemoteAnswerStateV1::Uncertain
            } else {
                RemoteAnswerStateV1::Queued
            };
            let mut outcome = json!({"code": if delivery.effect_started {"remote_answer_effect_uncertain"} else {"remote_answer_recovered_pre_effect"}});
            // Restart must preserve the bounded pre-effect retry budget.
            if let Some(retries) = delivery
                .receipt
                .outcome
                .as_ref()
                .and_then(|v| v["pre_effect_retries"].as_u64())
            {
                outcome["pre_effect_retries"] = json!(retries);
            }
            let receipt = RemoteAnswerReceiptV1 {
                receipt_key: delivery.receipt.receipt_key,
                state,
                outcome: Some(outcome),
            };
            tx.execute("UPDATE remote_answer_deliveries SET state=?1,claim_boot_id=NULL,outcome_json=?2,updated_at=?3 WHERE idempotency_key=?4",
                params![serde_json::to_value(state)?.as_str(),serde_json::to_string(&receipt)?,now(),key])?;
        }
        tx.commit()?;
        Ok(keys.len())
    }
}

#[cfg(test)]
#[path = "remote_answers/tests.rs"]
mod tests;
