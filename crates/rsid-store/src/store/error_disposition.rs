//! Daemon error disposition (#3427). No cleanup or hard deletion is performed.
use super::{Store, manager_actions::RecoveryOwnerMode};
use crate::error::Result;
use chrono::{DateTime, Utc};
use rsi_common::types::{
    ConversationEvent, EventType, Recurrence, ScheduleSpec, ScheduledJob, Session, SessionStatus,
    WakeMode,
};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use uuid::Uuid;

pub const PAGE_SIZE: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkEvidence {
    pub dirty: bool,
    pub commits: bool,
    pub landed: bool,
    /// Unknown evidence always prevents deletion/archival.
    pub unknown: bool,
    pub preserved_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Hold,
    Delete,
    Resume,
    Archive,
    Superseded,
    Notice,
}

#[derive(Debug)]
pub struct ErrorFacts {
    pub session: Session,
    pub turns: u32,
    pub terminal_at: DateTime<Utc>,
    pub superseded: bool,
    pub protected: bool,
    pub held: bool,
    pub settled: bool,
    pub transient: bool,
    pub heal_exhausted: bool,
    pub base: Option<String>,
    pub attempts: Vec<DateTime<Utc>>,
}

impl ErrorFacts {
    pub fn classify(&self, work: &WorkEvidence, budget: u32, now: DateTime<Utc>) -> Disposition {
        if self.held {
            return Disposition::Hold;
        }
        if self.superseded && !self.protected {
            return if work.unknown {
                Disposition::Notice
            } else {
                Disposition::Superseded
            };
        }
        if work.unknown {
            return Disposition::Notice;
        }
        let s = &self.session;
        if !self.protected
            && self.turns == 0
            && s.claude_session_id.is_none()
            && !work.commits
            && !work.dirty
        {
            return Disposition::Delete;
        }
        let cause = s.stop_reason.as_deref().unwrap_or("");
        let infra = cause.starts_with("daemon_restart_resume:")
            || cause.starts_with("interrupted:deploy")
            || cause == "deploy_interrupt"
            || (matches!(
                s.terminal_reason.as_deref(),
                Some("aborted_streaming" | "aborted_tools")
            ) && cause.contains("daemon"));
        if !self.settled
            && !work.landed
            && (infra || self.transient)
            && !self.heal_exhausted
            && self.attempts.len() < budget as usize
        {
            return Disposition::Resume;
        }
        if self.protected {
            return Disposition::Notice;
        }
        let owner_stop = matches!(
            cause,
            "interrupted:manager_halt"
                | "interrupted:graph_cancel"
                | "manager_halt"
                | "graph_cancel"
                | "terminal_handoff_superseded_by_tool"
        ) || (matches!(cause, "interrupted:operator" | "operator")
            && now - self.terminal_at >= chrono::Duration::hours(24));
        let final_failure = s.status == SessionStatus::Failed
            && (s.stop_reason.is_some() || s.terminal_reason.is_some());
        if self.settled
            || work.landed
            || owner_stop
            || final_failure
            || self.heal_exhausted
            || ((infra || self.transient) && self.attempts.len() >= budget as usize)
        {
            return Disposition::Archive;
        }
        Disposition::Notice
    }
}

impl Store {
    /// Keyset pagination: held rows cannot starve older projects indefinitely.
    pub fn error_disposition_page(
        &self,
        after: Option<Uuid>,
        before: DateTime<Utc>,
    ) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id FROM sessions s LEFT JOIN model_invocations m ON m.id=s.model_invocation_id
             WHERE s.status IN ('Failed','Interrupted') AND s.id>?1
             AND julianday(COALESCE(m.completed_at,s.updated_at))<=julianday(?2) ORDER BY s.id LIMIT ?3",
        )?;
        let ids = stmt.query_map(
            params![
                after.map(|id| id.to_string()).unwrap_or_default(),
                before.to_rfc3339(),
                PAGE_SIZE as i64
            ],
            |r| r.get::<_, String>(0),
        )?;
        ids.map(|id| {
            Uuid::parse_str(&id?).map_err(|e| crate::error::DaemonError::Store(e.to_string()))
        })
        .collect()
    }

    pub fn error_disposition_facts(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<ErrorFacts>> {
        let Some(session) = self.get_session(id)? else {
            return Ok(None);
        };
        if !matches!(
            session.status,
            SessionStatus::Failed | SessionStatus::Interrupted
        ) || !rsi_common::is_leaf_kind(session.session_kind)
        {
            return Ok(None);
        }
        let turns = (self.count_assistant_messages(id)?.max(0) as u32)
            .max(session.turn_count.unwrap_or(0))
            .max(session.num_turns.unwrap_or(0));
        let superseded: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE continued_from=?1)",
            [id.to_string()],
            |r| r.get(0),
        )?;
        let lead: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE lead_session_id=?1)",
            [id.to_string()],
            |r| r.get(0),
        )?;
        let protected = lead || self.error_disposition_manager_seat(id)?;
        let owned: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE enabled=1
               AND wake_mode IN ('resume','when') AND wake_session_id=?1)
             OR EXISTS(SELECT 1 FROM agent_jobs WHERE owner_session_id=?1 AND state IN ('queued','running'))
             OR EXISTS(SELECT 1 FROM manager_review_assignments WHERE (reviewer_session_id=?1 OR author_session_id=?1)
               AND state IN ('reserved','allocating','active'))
             OR EXISTS(SELECT 1 FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id
               WHERE a.session_id=?1 AND e.status IN ('accepted','running','cancelling')
               AND a.status IN ('reserved','launching','running','waiting'))",
            [id.to_string()], |r| r.get(0))?;
        let heal = self.transient_heal_state(super::transient_heal::transient_heal_job_id(id))?;
        let heal_exhausted = heal
            .as_ref()
            .is_some_and(|h| h.exhausted || h.window_expired(now));
        let heal_owned = heal
            .as_ref()
            .is_some_and(|h| !h.exhausted && !h.window_expired(now));
        let policy_paused = session
            .project_id
            .map(|project| self.get_harness_manager_policy(project))
            .transpose()?
            .flatten()
            .is_some_and(|grant| grant.policy.paused);
        let held = policy_paused
            || session.is_eval
            || owned
            || heal_owned
            || self.restart_intent_owns_session(id)?
            || session.retry_attempt.unwrap_or(0) < session.max_retries.unwrap_or(0)
            || self
                .recovery_owner_gate(id, RecoveryOwnerMode::AgentArchive)
                .is_err();
        let settled: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations o
               WHERE o.project_id=(SELECT project_id FROM sessions WHERE id=?1) AND o.target_session_id=?1
               AND EXISTS(SELECT 1 FROM issues i WHERE i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 AND i.project_id=o.project_id AND i.status IN ('Closed','Cancelled')))
             OR EXISTS(SELECT 1 FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id
               WHERE a.session_id=?1 AND e.status IN ('succeeded','failed','cancelled','blocked'))",
            [id.to_string()], |r| r.get(0))?;
        let base = self.conn.query_row(
            "SELECT c.source_commit FROM sessions s JOIN sandbox_custody_roots c ON c.custody_id=s.sandbox_custody_id WHERE s.id=?1",
            [id.to_string()], |r| r.get::<_, String>(0)).optional()?;
        let attempts: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM daemon_settings WHERE key=?1",
                [format!("error_disposition_attempts:{id}")],
                |r| r.get(0),
            )
            .optional()?;
        let attempts: Vec<DateTime<Utc>> = attempts
            .map(|v| serde_json::from_str(&v))
            .transpose()?
            .unwrap_or_default();
        let attempts = attempts
            .into_iter()
            .filter(|at| *at > now - chrono::Duration::hours(24))
            .collect();
        let transient = (session.status == SessionStatus::Failed
            || session.terminal_reason.as_deref() == Some("error_during_execution"))
            && self.transient_failure_verdict(&session)?.is_transient();
        let terminal_at: Option<String> = self.conn.query_row(
            "SELECT m.completed_at FROM sessions s LEFT JOIN model_invocations m ON m.id=s.model_invocation_id WHERE s.id=?1",
            [id.to_string()], |r| r.get(0))?;
        let terminal_at = terminal_at
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(session.updated_at);
        Ok(Some(ErrorFacts {
            session,
            turns,
            terminal_at,
            superseded,
            protected,
            held,
            settled,
            transient,
            heal_exhausted,
            base,
            attempts,
        }))
    }

    /// The snapshot and all ownership gates are checked again under the same
    /// transaction as the diagnosis and status/wake write. The caller holds the
    /// per-session spawn guard and has already preserved Git evidence.
    pub fn apply_error_disposition(
        &self,
        snapshot: &ErrorFacts,
        work: &WorkEvidence,
        budget: u32,
        grace_minutes: u32,
        draining: bool,
        now: DateTime<Utc>,
    ) -> Result<Disposition> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let Some(facts) = self.error_disposition_facts(snapshot.session.id, now)? else {
            return Ok(Disposition::Hold);
        };
        if facts.terminal_at > now - chrono::Duration::minutes(i64::from(grace_minutes)) {
            return Ok(Disposition::Hold);
        }
        if facts.session.updated_at != snapshot.session.updated_at
            || facts.session.sandbox_root != snapshot.session.sandbox_root
            || facts.session.sandbox_branch != snapshot.session.sandbox_branch
        {
            return Ok(Disposition::Hold);
        }
        let rule = facts.classify(work, budget, now);
        if matches!(rule, Disposition::Archive | Disposition::Superseded)
            && work.commits
            && !work.landed
            && work.preserved_ref.is_none()
        {
            return Ok(Disposition::Hold);
        }

        if rule == Disposition::Hold || (draining && rule == Disposition::Resume) {
            return Ok(Disposition::Hold);
        }
        let s = &facts.session;
        let key = format!("error_disposition_notice:{}", s.id);
        if rule == Disposition::Notice
            && self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1)",
                [&key],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Ok(Disposition::Hold);
        }
        let diagnosis = json!({"session_id":s.id, "project":s.project_id, "provider":s.provider,
            "model":s.model, "status":s.status, "stop_reason":s.stop_reason,
            "terminal_reason":s.terminal_reason, "turns":facts.turns,
            "age_seconds":(now-facts.terminal_at).num_seconds(), "rule":format!("{rule:?}"),
            "preserved_ref":work.preserved_ref});
        if rule == Disposition::Notice {
            // Return no change if there is no manager route, so a later appointment
            // can receive this notice. Never manufacture an operator to-do.
            if !self.record_error_disposition_notice(s, &diagnosis, now)? {
                return Ok(Disposition::Hold);
            }
            self.set_daemon_setting(&key, "true")?;
        }
        if rule == Disposition::Resume {
            let mut attempts = facts.attempts;
            attempts.push(now);
            let key = format!("error_disposition_attempts:{}", s.id);
            self.set_daemon_setting(&key, &serde_json::to_string(&attempts)?)?;
            let at = now + chrono::Duration::seconds(30 * (1_i64 << (attempts.len().min(8) - 1)));
            let job = ScheduledJob {
                id: Uuid::new_v5(
                    &Uuid::NAMESPACE_OID,
                    format!(
                        "error-disposition:{}:{}",
                        s.id,
                        now.timestamp_nanos_opt().unwrap_or_default()
                    )
                    .as_bytes(),
                ),
                name: format!("Error disposition {}", s.id),
                message:
                    "The daemon is resuming your interrupted work. Continue from your saved state."
                        .into(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: at,
                },
                last_fired_at: None,
                next_fire_at: at,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: s.project_id,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(s.id),
            };
            super::scheduled_jobs::insert_scheduled_job_conn(&tx, &job)?;
        }
        // System events and friction commit with the transition, so retries never
        // lose a diagnosis or increment a burst twice.
        let sequence = i32::try_from(self.conversation_max_sequence(s.id)?.saturating_add(1))
            .map_err(|_| {
                crate::error::DaemonError::Store("error disposition event sequence overflow".into())
            })?;
        let event = ConversationEvent {
            id: 0,
            session_id: s.id,
            sequence,
            event_type: EventType::System,
            role: None,
            content: format!("Error disposition: {rule:?}"),
            created_at: now,
            offload_id: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            metadata: Some(Box::new(json!({"error_disposition": diagnosis}))),
        };
        Self::insert_event_in_transaction(&tx, &event, None)?;
        let normalize = |v: &str| -> String {
            let mut code = String::from("v_");
            code.extend(
                v.chars()
                    .take(32)
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }),
            );
            code
        };
        let provider = normalize(&format!("{:?}", s.provider));
        let model = normalize(s.model.as_deref().unwrap_or("unknown"));
        let cause = normalize(s.stop_reason.as_deref().unwrap_or("unknown"));
        let friction = rsi_common::friction::NewFrictionEventV1::new(
            rsi_common::friction::FrictionKind::Terminal,
            &["error_disposition", &provider, &model, &cause],
        )
        .session(Some(s.id));
        super::friction::record_friction_in(&self.conn, &friction, now)?;
        match rule {
            Disposition::Delete => {
                self.soft_delete_session(s.id)?;
                tx.execute(
                    "DELETE FROM daemon_settings WHERE key=?1",
                    [super::daemon_settings::c5_autofile_pending_key(s.id)],
                )?;
            }
            Disposition::Archive | Disposition::Superseded => {
                self.update_session_status(s.id, SessionStatus::Archived)?;
                tx.execute(
                    "DELETE FROM daemon_settings WHERE key=?1",
                    [super::daemon_settings::c5_autofile_pending_key(s.id)],
                )?;
                Self::cancel_queued_recovery_for_archive_on(&tx, s.id)?;
            }
            _ => {}
        }
        tx.commit()?;
        Ok(rule)
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
#[path = "error_disposition_tests.rs"]
mod tests;
