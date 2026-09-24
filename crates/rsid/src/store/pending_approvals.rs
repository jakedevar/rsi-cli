//! Producer-bound native approvals. Legacy approval rows never supply provenance.
use super::{
    Store,
    harness_manager_v2::{fingerprint, now, refused},
};
use crate::error::Result;
use rsi_common::{harness_manager::HarnessManagerConfigV1, types::ConversationEvent};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

#[cfg(test)]
pub(crate) use super::manager_coordinator::tests::fixture as approval_test_fixture;

pub(crate) const MAX_PENDING_APPROVALS: usize = 64;

pub(crate) fn approval_decision_key(target: &Value) -> String {
    format!(
        "approval:{}",
        target["publication_id"].as_str().unwrap_or_default()
    )
}

impl Store {
    /// Each occurrence owns its mirror and journal target. Only another
    /// occurrence of the same typed request in this incarnation supersedes it.
    pub(crate) fn publish_appserver_approval(
        &self,
        event: &ConversationEvent,
        target: &Value,
    ) -> Result<i64> {
        let session = event.session_id.to_string();
        let publication = target["publication_id"]
            .as_str()
            .ok_or_else(|| refused("approval_identity_missing"))?;
        let incarnation = target["incarnation_id"]
            .as_str()
            .ok_or_else(|| refused("approval_identity_missing"))?;
        let request = if target["request_id"].is_i64() || target["request_id"].is_string() {
            target["request_id"].to_string()
        } else {
            json!({"unresolved_publication":publication}).to_string()
        };
        let approval =
            Uuid::parse_str(publication).map_err(|_| refused("approval_identity_missing"))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stamp = now();
        // The bounded runtime allows one explicit overflow record, then seals
        // this writer. Store applies the same bound if called independently.
        let pending: i64 = tx.query_row("SELECT count(*) FROM (SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND incarnation_id=?2 AND state IN ('unresolved','published','enqueued') AND closure_state<>'closed' AND request_id_json<>?3 LIMIT 65)",params![session,incarnation,request],|r|r.get(0))?;
        if pending > MAX_PENDING_APPROVALS as i64
            || (pending == MAX_PENDING_APPROVALS as i64 && target["overflow"] != true)
        {
            return Err(refused("approval_pending_capacity_exceeded"));
        }
        tx.execute("UPDATE appserver_approval_publications SET state='superseded',outcome='new occurrence of this exact typed request; old answers invalidated',updated_at=?4 WHERE session_id=?1 AND incarnation_id=?2 AND request_id_json=?3 AND state IN ('unresolved','published','enqueued') AND closure_state<>'closed'",params![session,incarnation,request,stamp])?;
        tx.execute("INSERT INTO approvals(id,session_id,tool_name,tool_input,status,created_at) VALUES(?1,?2,?3,?4,'Pending',?5)",params![approval.to_string(),session,target["method"].as_str().unwrap_or("AppServer approval"),target.to_string(),stamp])?;
        tx.execute("INSERT INTO appserver_approval_publications(publication_id,session_id,incarnation_id,request_id_json,thread_id,approval_id,state,target_json,outcome,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?1,'unresolved',?6,?7,?8,?8)",params![publication,session,incarnation,request,target["params"]["threadId"].as_str(),target.to_string(),if target["overflow"]==true {"pending approval capacity exceeded; provider ingress sealed; no gate evicted"} else {"approval event publication incomplete"},stamp])?;
        tx.execute(
            "UPDATE sessions SET status='WaitingApproval',updated_at=?2 WHERE id=?1",
            params![session, stamp],
        )?;
        tx.commit()?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let event_id = Self::insert_event_in_transaction(&tx, event, None)?;
        let mut bound = target.clone();
        bound["event_id"] = json!(event_id);
        let valid = target["overflow"] != true && self.appserver_approval_binding_valid(&bound)?;
        let changed = tx.execute("UPDATE appserver_approval_publications SET target_json=?1,state=?2,outcome=?3,updated_at=?4 WHERE publication_id=?5 AND incarnation_id=?6 AND state='unresolved' AND closure_state='open'",params![bound.to_string(),if valid {"published"} else {"unresolved"},if valid {None} else if target["overflow"]==true {Some("pending approval capacity exceeded; provider ingress sealed; no gate evicted")} else {Some("unsupported or incomplete provider approval identity")},now(),publication,incarnation])?;
        if changed != 1 {
            return Err(refused("approval_publication_changed"));
        }
        tx.commit()?;
        Ok(event_id)
    }

    fn appserver_approval_binding_valid(&self, target: &Value) -> Result<bool> {
        let valid_method = target["method"].as_str().is_some_and(|m| {
            [
                crate::provider::ApprovalDecision::Approve,
                crate::provider::ApprovalDecision::Deny,
            ]
            .into_iter()
            .any(|answer| {
                crate::codex_app_server::approval_response(
                    &target["request_id"],
                    m,
                    &target["params"],
                    answer,
                )
                .is_ok()
            })
        });
        if target["kind"] != "appserver_approval"
            || !valid_method
            || target["payload_complete"] != true
            || target["event_id"].as_i64().is_none_or(|id| id <= 0)
        {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions s JOIN model_invocations mi ON mi.session_id=s.id
            JOIN conversation_events ce ON ce.session_id=s.id AND ce.id=?4
            WHERE s.id=?1 AND s.provider='CodexAppServer' AND s.status NOT IN ('Archived','Deleted')
            AND s.model_invocation_id=?2 AND mi.id=?3 AND mi.admission_status='admitted'
            AND mi.status IN ('running','cancellation_requested')
            AND ce.event_type='ToolUse' AND ce.role='Assistant' AND ce.tool_use_id=?5
            AND ce.tool_name=?6 AND ce.sequence=?7
            AND json_extract(ce.tool_input,'$.incarnation_id')=?8
            AND json_extract(ce.tool_input,'$.model_invocation_id')=mi.id
            AND json_extract(ce.tool_input,'$.request_id')=json_extract(?9,'$')
            AND json_type(ce.tool_input,'$.request_id')=json_type(?9,'$'))",
            params![
                target["session_id"].as_str(),
                target["launch_invocation_id"].as_str(),
                target["model_invocation_id"].as_str(),
                target["event_id"].as_i64(),
                target["publication_id"].as_str(),
                target["method"].as_str(),
                target["event_sequence"].as_i64(),
                target["incarnation_id"].as_str(),
                target["request_id"].to_string()
            ],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn appserver_approval_target(
        &self,
        session: Uuid,
        publication: &str,
    ) -> Result<Option<Value>> {
        let row: Option<(String,String,String)> = self.conn.query_row("SELECT state,closure_state,target_json FROM appserver_approval_publications WHERE session_id=?1 AND publication_id=?2",params![session.to_string(),publication],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((state, closure, raw)) = row else {
            return Ok(None);
        };
        if closure == "closed" || state == "superseded" || state == "enqueued" {
            return Ok(None);
        }
        let target: Value = serde_json::from_str(&raw)?;
        if state != "published"
            || closure != "open"
            || !self.appserver_approval_binding_valid(&target)?
        {
            return Err(refused(
                "manager_v2_approval_writer_or_provenance_unavailable",
            ));
        }
        Ok(Some(target))
    }

    // Compatibility for single-gate callers; never chooses one of several.
    #[cfg(test)]
    pub(crate) fn pending_appserver_approval_target(&self, session: Uuid) -> Result<Option<Value>> {
        let mut stmt = self.conn.prepare("SELECT publication_id FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state NOT IN ('superseded','enqueued') LIMIT 2")?;
        let ids = stmt
            .query_map([session.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match ids.as_slice() {
            [] => Ok(None),
            [id] => self.appserver_approval_target(session, id),
            _ => Err(refused("manager_v2_exact_approval_publication_required")),
        }
    }

    pub(crate) fn expire_appserver_approvals(&self, incarnations: &[Uuid]) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let stamp = now();
        let changed = tx.execute("UPDATE appserver_approval_publications SET state='expired',outcome=CASE WHEN state='enqueued' THEN 'response enqueued; consumption unconfirmed; writer lost; no resend' ELSE 'live AppServer writer unavailable; inspect exact occurrence' END,updated_at=?1 WHERE publication_id IN (SELECT publication_id FROM appserver_approval_publications WHERE state IN ('published','unresolved','enqueued') AND closure_state<>'closed' AND incarnation_id NOT IN (SELECT value FROM json_each(?2)) ORDER BY updated_at,publication_id LIMIT 64)",params![stamp,serde_json::to_string(incarnations)?])?;
        tx.commit()?;
        Ok(changed)
    }

    /// Source-bound closure has its own durable intent. A failed event write
    /// leaves an ambiguous non-answerable gate; replay completes only this
    /// occurrence. Provider closure never changes the answer delivery journal.
    pub(crate) fn resolve_appserver_approval(
        &self,
        event: &ConversationEvent,
        target: &Value,
        resolution: &Value,
    ) -> Result<bool> {
        let session = event.session_id.to_string();
        let publication = target["publication_id"].as_str();
        let incarnation = target["incarnation_id"].as_str();
        let thread = resolution["threadId"].as_str();
        if thread.is_none()
            || resolution["requestId"] != target["request_id"]
            || thread != target["params"]["threadId"].as_str()
        {
            return Err(refused("approval_resolution_identity_mismatch"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row: Option<(String,String,String,Option<String>)> = tx.query_row("SELECT state,closure_state,target_json,closure_json FROM appserver_approval_publications WHERE session_id=?1 AND publication_id=?2 AND incarnation_id=?3 AND thread_id=?4",params![session,publication,incarnation,thread],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let Some((state, closure, raw, previous)) = row else {
            return Err(refused("approval_publication_changed"));
        };
        if serde_json::from_str::<Value>(&raw)? != *target
            || matches!(state.as_str(), "expired" | "superseded")
        {
            return Err(refused("approval_publication_changed"));
        }
        if let Some(previous) = previous {
            let previous: Value = serde_json::from_str(&previous)?;
            if previous["notification"] == *resolution
                && previous["event_id"].as_i64().is_some_and(|id| id > 0)
            {
                tx.commit()?;
                return Ok(closure == "closed");
            }
        }
        let reused: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND incarnation_id=?2 AND request_id_json=?3 AND publication_id<>?4)",params![session,incarnation,target["request_id"].to_string(),publication],|r|r.get(0))?;
        let mut evidence = json!({"source":"serverRequest/resolved","publication_id":publication,"incarnation_id":incarnation,"notification":resolution,"event_id":0,"request_id_reused":reused});
        tx.execute("UPDATE appserver_approval_publications SET closure_state='ambiguous',closure_json=?2,outcome='provider closure observed; persistence pending; no answer allowed',updated_at=?3 WHERE publication_id=?1",params![publication,evidence.to_string(),now()])?;
        tx.commit()?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let event_id = Self::insert_event_in_transaction(&tx, event, None)?;
        evidence["event_id"] = json!(event_id);
        let changed = tx.execute("UPDATE appserver_approval_publications SET closure_state=?2,closure_json=?3,closed_at=?4,outcome=?5,updated_at=?6 WHERE publication_id=?1 AND target_json=?7 AND state NOT IN ('expired','superseded')",params![publication,if reused {"ambiguous"} else {"closed"},evidence.to_string(),if reused {None} else {Some(now())},if reused {"request ID reused in this incarnation; closure occurrence ambiguous; no answer or automatic resend"} else {"provider request closed (answered or cleared); consumption of a particular operator answer is not established"},now(),target.to_string()])?;
        if changed != 1 {
            return Err(refused("approval_publication_changed"));
        }
        self.sync_appserver_approval_status(event.session_id)?;
        tx.commit()?;
        Ok(!reused)
    }

    pub(crate) fn appserver_approval_waiting(&self, session: Uuid) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM appserver_approval_publications WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded') OR EXISTS(SELECT 1 FROM approvals a WHERE a.session_id=?1 AND a.status='Pending' AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p WHERE p.approval_id=a.id))",[session.to_string()],|r|r.get(0))?)
    }

    fn sync_appserver_approval_status(&self, session: Uuid) -> Result<()> {
        if !self.appserver_approval_waiting(session)? {
            self.conn.execute("UPDATE sessions SET status='Running',updated_at=?2 WHERE id=?1 AND status='WaitingApproval' AND pending_question_json IS NULL AND pending_archive=0",params![session.to_string(),now()])?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // Bounded terminal selection and the existing decision projection share one transaction.
    pub(crate) fn manager_v2_refresh_approval_decisions(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<usize> {
        let cohort = self.manager_v2_cohort(config)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut changed = 0;
        let mut sessions = std::collections::BTreeSet::new();
        sessions.extend(
            cohort
                .iter()
                .filter(|m| m.epic_id.is_some())
                .map(|m| m.session.id),
        );
        // A retired session may still own an unresolved publication. Select
        // only terminal sessions with a missing or stale decision projection;
        // stable blocked projections do not consume each refresh batch.
        let mut stmt = tx.prepare(
            "SELECT DISTINCT p.session_id FROM appserver_approval_publications p
             JOIN sessions s ON s.id=p.session_id
             LEFT JOIN harness_manager_v2_records r ON r.project_id=?1
               AND r.manager_session_id=?2 AND r.scope_version=?3 AND r.kind='decision'
               AND r.record_key='approval:'||p.publication_id
             WHERE s.project_id=?1
               AND s.status IN ('Completed','Failed','Interrupted','Archived','Deleted')
               AND (s.parent_id IN (SELECT value FROM json_each(?4))
                    OR r.epic_id IN (SELECT value FROM json_each(?4)))
               AND (r.record_key IS NULL
                    OR json_extract(r.payload_json,'$.publication_stamp') IS NOT p.updated_at
                    OR json_extract(r.payload_json,'$.status') IN
                       ('pending','answer_queued','answer_sent')
                    OR (s.status IN ('Archived','Deleted') AND r.epic_id IS NOT NULL))
             ORDER BY p.session_id LIMIT 128",
        )?;
        let retired = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    serde_json::to_string(&config.epic_ids)?
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for id in retired {
            sessions.insert(
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
            );
        }
        for session in sessions {
            let epic = self.manager_v2_live_epic_for_session(config, session).ok();
            // Active gates have priority; historical closure changes drain in
            // bounded batches. Stable terminal projections are not rewritten.
            let mut stmt = tx.prepare("SELECT p.state,p.closure_state,p.target_json,p.outcome,p.updated_at,p.closure_json FROM appserver_approval_publications p LEFT JOIN harness_manager_v2_records r ON r.project_id=?2 AND r.manager_session_id=?3 AND r.scope_version=?4 AND r.kind='decision' AND r.record_key='approval:'||p.publication_id WHERE p.session_id=?1 AND ((p.state IN ('unresolved','published','enqueued') AND p.closure_state='open') OR json_extract(r.payload_json,'$.publication_stamp') IS NOT p.updated_at OR r.epic_id IS NOT ?5) ORDER BY CASE WHEN p.state='published' AND p.closure_state='open' THEN 0 ELSE 1 END,p.publication_id LIMIT 128")?;
            let rows = stmt
                .query_map(
                    params![
                        session.to_string(),
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        epic.map(|e| e.to_string())
                    ],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, Option<String>>(5)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            for (state, closure, raw, outcome, stamp, closure_json) in rows {
                let target: Value = serde_json::from_str(&raw)?;
                let key = approval_decision_key(&target);
                let digest = fingerprint(&target)?;
                let old = self.manager_v2_record(config, "decision", &key)?;
                let answerable = epic.is_some()
                    && self
                        .appserver_approval_target(
                            session,
                            target["publication_id"].as_str().unwrap_or_default(),
                        )
                        .is_ok_and(|t| t.is_some());
                let same = old
                    .as_ref()
                    .filter(|r| r.epic_id == epic && r.payload["target_digest"] == digest);
                let mut status = if closure == "closed" {
                    "resolved"
                } else if state == "superseded" {
                    "superseded"
                } else if answerable {
                    "pending"
                } else {
                    "blocked"
                };
                if let Some(old) = same {
                    if closure != "closed"
                        && state != "superseded"
                        && (answerable || state == "enqueued")
                        && matches!(
                            old.payload["status"].as_str(),
                            Some("answer_queued" | "answer_sent")
                        )
                    {
                        status = old.payload["status"].as_str().unwrap();
                    }
                }
                let answers: Vec<_> = [
                    ("approve", crate::provider::ApprovalDecision::Approve),
                    ("deny", crate::provider::ApprovalDecision::Deny),
                ]
                .into_iter()
                .filter_map(|(label, answer)| {
                    (answerable
                        && crate::codex_app_server::approval_response(
                            &target["request_id"],
                            target["method"].as_str().unwrap_or_default(),
                            &target["params"],
                            answer,
                        )
                        .is_ok())
                    .then_some(label)
                })
                .collect();
                self.manager_v2_record_changed(config, "decision_target", &key, epic, &target)?;
                let mut payload = json!({"key":key,"epic_id":epic,"session_id":session,"question":format!("AppServer approval {} (request {}): {}",target["method"],target["request_id"],target["description"]),"available_answers":answers,"request_id":null,"work_key":null,"target_digest":digest,"status":status,"answer":null,"delivery":null,"route_state":if answerable {"live_appserver_writer"} else if closure=="closed" {"closed"} else {"unavailable"},"provider_request":target,"outcome":outcome,"publication_stamp":stamp,"closure_state":closure,"closure_evidence":closure_json.map(|raw|serde_json::from_str::<Value>(&raw)).transpose()?,"next_action":if answerable {"Answer this exact operator decision."} else if closure=="closed" {"Provider request closed; inspect separate answer delivery evidence."} else {"Inspect this exact occurrence; no automatic resend."}});
                if let Some(old) = same {
                    payload["delivery"] = old.payload["delivery"].clone();
                    payload["answer"] = old.payload["answer"].clone();
                }
                changed += usize::from(
                    self.manager_v2_record_changed(config, "decision", &key, epic, &payload)?,
                );
            }
            // A V106 decision never inherits a current writer from migration.
            let legacy_key = format!("approval:{session}");
            if let Some(old) = self.manager_v2_record(config, "decision", &legacy_key)? {
                if old.payload["provider_request"]["kind"] == "appserver_approval"
                    && old.payload["route_state"] != "legacy_unavailable"
                {
                    let mut payload = old.payload;
                    payload["status"] = json!("blocked");
                    payload["route_state"] = json!("legacy_unavailable");
                    payload["outcome"] = json!(
                        "historical V106 decision; inspect independent occurrence records; no inherited writer authority"
                    );
                    changed += usize::from(self.manager_v2_record_changed(
                        config,
                        "decision",
                        &legacy_key,
                        old.epic_id,
                        &payload,
                    )?);
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Enqueue settles only this answer journal. It does not close the provider
    /// request or release WaitingApproval; only exact provider closure does so.
    pub(crate) fn finish_appserver_approval_enqueue(
        &self,
        delivery: &super::manager_coordinator::ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let config = self.manager_v2_check_decision_target(delivery)?;
        let record = self
            .manager_v2_record(&config, "decision_delivery", &delivery.key)?
            .ok_or_else(|| refused("manager_v2_decision_claim_changed"))?;
        if record.payload != serde_json::to_value(delivery)?
            || !delivery.effect_started
            || delivery.state != "running"
        {
            return Err(refused("manager_v2_decision_claim_changed"));
        }
        let changed = tx.execute("UPDATE appserver_approval_publications SET state='enqueued',outcome='response enqueued; provider consumption unconfirmed',updated_at=?1 WHERE session_id=?2 AND publication_id=?3 AND state='published' AND closure_state='open' AND target_json=?4",params![now(),delivery.target["session_id"].as_str(),delivery.target["publication_id"].as_str(),delivery.target.to_string()])?;
        if changed != 1 {
            return Err(refused("approval_publication_changed"));
        }
        tx.execute("UPDATE approvals SET status=?1,resolved_at=?2 WHERE id=(SELECT approval_id FROM appserver_approval_publications WHERE publication_id=?3)",params![if delivery.answer.trim()=="approve" {"Approved"} else {"Denied"},now(),delivery.target["publication_id"].as_str()])?;
        let mut finished = delivery.clone();
        finished.state = "enqueued".into();
        finished.outcome=Some("response enqueued to the exact live writer; provider consumption unconfirmed; never automatically resent".into());
        self.manager_v2_delivery_transition_on(&config, &record, &finished)?;
        tx.commit()?;
        Ok(())
    }
}
