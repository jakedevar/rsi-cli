//! Heal scheduling for sessions that ended `Failed` for a transient reason
//! (Issue #1015, slices C and D).
//!
//! `schedule_transient_heal` is called once from the finalizer after a session
//! settles `Failed`. It classifies the failure
//! ([`crate::model_control::transient_heal`]), checks eligibility, then
//! creates or re-arms one deterministic `Resume` scheduled job per session
//! with the K2 backoff. The attempt counter lives at
//! `schedule_json.$.transient_heal`, distinct from the continuation-fence
//! retry namespace `$.continuation_retry`. The daemon scheduler's existing
//! `Resume` branch performs the resume (and, since #996, retains the wake on a
//! capacity or pause refusal).
//!
//! The job row is the durable counter across provider refusals, rotations and
//! daemon restarts. Retention preserves heal rows so it cannot reset a budget.

use chrono::{DateTime, Utc};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, Session, SessionStatus, WakeMode};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use uuid::Uuid;

use super::Store;
use super::manager_actions::RecoveryOwnerMode;
use super::scheduled_jobs::continuation_backoff_seconds;
use crate::error::Result;
use crate::model_control::transient_heal::{
    HealIneligible, TransientHealObservation, TransientVerdict,
    classify_transient_failure_with_stop, heal_eligibility,
};

/// The Issue #1015 budget includes both provider failures and refused heals.
pub const TRANSIENT_HEAL_MAX_ATTEMPTS: u32 = 8;
const TRANSIENT_HEAL_WINDOW_MINUTES: i64 = 60;
const RECENT_ERROR_TEXT_EVENTS: i64 = 3;
const RECENT_ERROR_TEXT_CHARS: usize = 512;

/// Daemon-owned heal state (`$.transient_heal`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransientHealV1 {
    pub attempts: u32,
    pub first_failed_at: String,
    pub last_reason: String,
    pub owner_session_id: Option<Uuid>,
    #[serde(default)]
    pub exhausted: bool,
    /// When the last budget-spending attempt was armed (#1082). Absent on
    /// rows written before the field existed; `first_failed_at` stands in.
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    /// Start of a pause/capacity refusal streak (#1082). A refusal spends no
    /// budget and the wall-clock window stops counting until the next attempt.
    #[serde(default)]
    pub paused_at: Option<String>,
}

fn parse_rfc3339(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

impl TransientHealV1 {
    /// True when the wall-clock budget is spent. Time a pause/capacity refusal
    /// kept the heal from running does not count. An unparseable
    /// `first_failed_at` never expires (the wake is retained).
    pub fn window_expired(&self, now: DateTime<Utc>) -> bool {
        let Some(first) = parse_rfc3339(&self.first_failed_at) else {
            return false;
        };
        let end = self
            .paused_at
            .as_deref()
            .and_then(parse_rfc3339)
            .unwrap_or(now);
        end - first >= chrono::Duration::minutes(TRANSIENT_HEAL_WINDOW_MINUTES)
    }
}

/// One scheduled (or exhausted) heal, for the owner notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealSchedule {
    pub session_id: Uuid,
    pub job_id: Uuid,
    pub attempt: u32,
    pub max_attempts: u32,
    /// `None` when the budget is exhausted and nothing was scheduled.
    pub not_before: Option<DateTime<Utc>>,
    pub reason: String,
    pub owner_session_id: Option<Uuid>,
    /// A manager-covered lead's notice was recorded in the manager inbox.
    /// `false` means the caller publishes the child-path bus event.
    pub manager_notified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransientHealOutcome {
    NotTransient(&'static str),
    Ineligible(HealIneligible),
    Scheduled(HealSchedule),
    /// A pause or capacity refusal re-armed the wake without spending budget.
    Deferred(HealSchedule),
    Exhausted(HealSchedule),
}

/// Deterministic per-session heal job id.
pub fn transient_heal_job_id(session_id: Uuid) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("rsi:transient-heal:{session_id}").as_bytes(),
    )
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

impl Store {
    /// The heal state stored on `job_id`, if any.
    pub fn transient_heal_state(&self, job_id: Uuid) -> Result<Option<TransientHealV1>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT CASE WHEN json_valid(schedule_json)
                        THEN json_extract(schedule_json,'$.transient_heal') END
                 FROM scheduled_jobs WHERE id=?1",
                params![job_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        raw.flatten()
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|error| crate::error::DaemonError::Store(error.to_string()))
            })
            .transpose()
    }

    fn transient_heal_observation(&self, session: &Session) -> Result<TransientHealObservation> {
        let is_lineage_tip = self.published_lineage_tip(session.id)? == Some(session.id);
        let (mut is_manager_seat_tip, mut paused) = (false, false);
        if let Some(project) = session.project_id {
            is_manager_seat_tip = self
                .get_harness_manager_notice_config(project)?
                .and_then(|config| config.current_session_id)
                == Some(session.id);
            if let Some(grant) = self.get_harness_manager_policy(project)? {
                paused = grant.revoked || grant.policy.paused;
            }
        }
        let has_enabled_resume_job: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
              WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1)",
            params![session.id.to_string()],
            |row| row.get(0),
        )?;
        // The strictest gate that does not hold on the freshly staged C5
        // autofile marker this failure just wrote: pending questions and
        // approvals, operator pause, program custody.
        let human_gated = self
            .recovery_owner_gate(session.id, RecoveryOwnerMode::AgentArchive)
            .is_err();
        Ok(TransientHealObservation {
            status: session.status,
            kind: session.session_kind,
            is_lineage_tip,
            is_manager_seat_tip,
            max_retries: session.max_retries,
            retry_attempt: session.retry_attempt,
            has_enabled_resume_job,
            paused,
            human_gated,
        })
    }

    fn recent_error_texts(&self, session_id: Uuid) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM conversation_events
             WHERE session_id=?1 AND event_type='System'
             ORDER BY sequence DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(
                params![session_id.to_string(), RECENT_ERROR_TEXT_EVENTS],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|text| text.chars().take(RECENT_ERROR_TEXT_CHARS).collect())
            .collect())
    }

    pub(crate) fn transient_failure_verdict(&self, session: &Session) -> Result<TransientVerdict> {
        let error_class: Option<String> = self
            .conn
            .query_row(
                "SELECT m.error_class FROM sessions s
                 LEFT JOIN model_invocations m ON m.id=s.model_invocation_id
                 WHERE s.id=?1",
                params![session.id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let texts = self.recent_error_texts(session.id)?;
        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        Ok(classify_transient_failure_with_stop(
            session.terminal_reason.as_deref(),
            session.stop_reason.as_deref(),
            error_class.as_deref(),
            &text_refs,
        ))
    }

    /// Classify a `Failed` session and, when it is a transient failure that
    /// may heal, create or re-arm its `Resume` job and record the owner
    /// notice. Idempotent: an armed job makes a repeat call ineligible.
    pub fn schedule_transient_heal(
        &self,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<TransientHealOutcome> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(TransientHealOutcome::Ineligible(HealIneligible::NotFailed));
        };
        if session.status != SessionStatus::Failed {
            return Ok(TransientHealOutcome::Ineligible(HealIneligible::NotFailed));
        }
        let reason = match self.transient_failure_verdict(&session)? {
            TransientVerdict::Transient { reason } => reason,
            TransientVerdict::NotTransient { reason } => {
                return Ok(TransientHealOutcome::NotTransient(reason));
            }
        };
        let observation = self.transient_heal_observation(&session)?;
        if let Err(ineligible) = heal_eligibility(&observation) {
            return Ok(TransientHealOutcome::Ineligible(ineligible));
        }

        // Rotation changes the session id, but must not reset the heal budget.
        let prior_job: Option<String> = self
            .conn
            .query_row(
                "WITH RECURSIVE ancestors(id) AS (
                SELECT ?1 UNION SELECT s.continued_from FROM sessions s
                JOIN ancestors a ON s.id=a.id WHERE s.continued_from IS NOT NULL)
             SELECT j.id FROM scheduled_jobs j JOIN ancestors a ON j.wake_session_id=a.id
             WHERE json_valid(j.schedule_json)
               AND json_type(j.schedule_json,'$.transient_heal')='object'
             ORDER BY j.updated_at DESC LIMIT 1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let job_id = prior_job
            .map(|id| {
                Uuid::parse_str(&id).map_err(|e| crate::error::DaemonError::Store(e.to_string()))
            })
            .transpose()?
            .unwrap_or_else(|| transient_heal_job_id(session_id));
        if self
            .get_scheduled_job(&job_id)?
            .is_some_and(|job| job.enabled)
        {
            return Ok(TransientHealOutcome::Ineligible(
                HealIneligible::ResumeJobExists,
            ));
        }
        self.record_transient_heal_attempt(session_id, job_id, reason, now, true)
    }

    /// Whether the previous budget epoch is over (#1082): the target ran
    /// healthy after the last attempt (non-System activity later than that
    /// attempt plus the longest backoff, so a retry that failed again does not
    /// count), or an exhausted row sat quiet for a whole window.
    fn transient_heal_epoch_over(
        &self,
        state: &TransientHealV1,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let Some(last_attempt) = state
            .last_attempt_at
            .as_deref()
            .and_then(parse_rfc3339)
            .or_else(|| parse_rfc3339(&state.first_failed_at))
        else {
            return Ok(false);
        };
        if state.exhausted
            && now - last_attempt >= chrono::Duration::minutes(TRANSIENT_HEAL_WINDOW_MINUTES)
        {
            return Ok(true);
        }
        let healthy_after =
            last_attempt + chrono::Duration::seconds(continuation_backoff_seconds(u32::MAX));
        let latest: Option<String> = self.conn.query_row(
            "SELECT MAX(created_at) FROM conversation_events
             WHERE session_id=?1 AND event_type<>'System'",
            params![session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(latest
            .as_deref()
            .and_then(parse_rfc3339)
            .is_some_and(|at| at > healthy_after))
    }

    /// A scheduler refusal of the heal's resume. With `spend_budget` it is
    /// another attempt (shared bounded counter and owner notice). Without it
    /// (a pause or capacity refusal: the session never ran) the wake is re-armed
    /// with the current backoff, no attempt is spent and no notice is sent.
    pub fn defer_transient_heal(
        &self,
        job_id: Uuid,
        reason: &str,
        now: DateTime<Utc>,
        spend_budget: bool,
    ) -> Result<TransientHealOutcome> {
        let job = self
            .get_scheduled_job(&job_id)?
            .ok_or_else(|| crate::error::DaemonError::Store("heal job missing".into()))?;
        let session_id = job
            .wake_session_id
            .ok_or_else(|| crate::error::DaemonError::Store("heal target missing".into()))?;
        let tip = self
            .published_lineage_tip(session_id)?
            .unwrap_or(session_id);
        // Dispatch and retry settlement are separate async boundaries. An
        // operator may have resumed/interrupted/archived the target meanwhile;
        // retire this obligation rather than re-arm or notify for that row.
        let Some(session) = self
            .get_session(tip)?
            .filter(|s| s.status == SessionStatus::Failed)
        else {
            self.conn.execute(
                "UPDATE scheduled_jobs SET enabled=0, updated_at=?2 WHERE id=?1",
                params![job_id.to_string(), rfc3339(now)],
            )?;
            return Ok(TransientHealOutcome::Ineligible(HealIneligible::NotFailed));
        };
        if let TransientVerdict::NotTransient { reason } =
            self.transient_failure_verdict(&session)?
        {
            self.conn.execute(
                "UPDATE scheduled_jobs SET enabled=0, updated_at=?2 WHERE id=?1",
                params![job_id.to_string(), rfc3339(now)],
            )?;
            return Ok(TransientHealOutcome::NotTransient(reason));
        }
        self.record_transient_heal_attempt(tip, job_id, reason, now, spend_budget)
    }

    fn rearm_without_spend(
        &self,
        session_id: Uuid,
        job_id: Uuid,
        previous: &TransientHealV1,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<TransientHealOutcome> {
        let attempt = previous.attempts.max(1);
        let not_before = now + chrono::Duration::seconds(continuation_backoff_seconds(attempt));
        let mut state = previous.clone();
        state.paused_at.get_or_insert_with(|| rfc3339(now));
        let state_json = serde_json::to_string(&state)
            .map_err(|error| crate::error::DaemonError::Store(error.to_string()))?;
        self.conn.execute(
            "UPDATE scheduled_jobs
             SET enabled=1, next_fire_at=?2, updated_at=?3,
                 schedule_json=json_remove(
                     json_set(schedule_json,'$.transient_heal',json(?4)),
                     '$.continuation_retry')
             WHERE id=?1 AND json_valid(schedule_json)",
            params![
                job_id.to_string(),
                rfc3339(not_before),
                rfc3339(now),
                state_json
            ],
        )?;
        Ok(TransientHealOutcome::Deferred(HealSchedule {
            session_id,
            job_id,
            attempt,
            max_attempts: TRANSIENT_HEAL_MAX_ATTEMPTS,
            not_before: Some(not_before),
            reason: reason.to_string(),
            owner_session_id: previous.owner_session_id,
            manager_notified: false,
        }))
    }

    fn record_transient_heal_attempt(
        &self,
        session_id: Uuid,
        job_id: Uuid,
        reason: &str,
        now: DateTime<Utc>,
        spend_budget: bool,
    ) -> Result<TransientHealOutcome> {
        let session = self
            .get_session(session_id)?
            .ok_or_else(|| crate::error::DaemonError::Store("heal session missing".into()))?;
        let mut previous = self.transient_heal_state(job_id)?;
        if spend_budget
            && let Some(state) = previous.as_ref()
            && self.transient_heal_epoch_over(state, session_id, now)?
        {
            previous = None;
        }
        if previous.as_ref().is_some_and(|state| state.exhausted) {
            return Ok(TransientHealOutcome::Ineligible(
                HealIneligible::BudgetExhausted,
            ));
        }
        if !spend_budget && let Some(state) = previous.as_ref() {
            return self.rearm_without_spend(session_id, job_id, state, reason, now);
        }
        let mut first_failed_at = previous
            .as_ref()
            .and_then(|state| parse_rfc3339(&state.first_failed_at))
            .unwrap_or(now);
        if let Some(paused_at) = previous
            .as_ref()
            .and_then(|state| state.paused_at.as_deref())
            .and_then(parse_rfc3339)
        {
            // Time spent refused by a pause or capacity limit is not budget.
            first_failed_at += (now - paused_at).max(chrono::Duration::zero());
        }
        let attempt = previous.as_ref().map_or(0, |state| state.attempts) + 1;
        let owner = session.parent_id;
        let mut schedule = HealSchedule {
            session_id,
            job_id,
            attempt,
            max_attempts: TRANSIENT_HEAL_MAX_ATTEMPTS,
            not_before: None,
            reason: reason.to_string(),
            owner_session_id: owner,
            manager_notified: false,
        };

        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        if attempt > TRANSIENT_HEAL_MAX_ATTEMPTS
            || now - first_failed_at >= chrono::Duration::minutes(TRANSIENT_HEAL_WINDOW_MINUTES)
        {
            tx.execute(
                "UPDATE scheduled_jobs SET enabled=0, updated_at=?2,
                    schedule_json=json_set(schedule_json,'$.transient_heal.exhausted',json('true'))
                 WHERE id=?1",
                params![job_id.to_string(), rfc3339(now)],
            )?;
            schedule.manager_notified = self.record_manager_heal_notice(
                session_id,
                "transient_heal_exhausted",
                &json!({
                    "attempt": attempt,
                    "max_attempts": TRANSIENT_HEAL_MAX_ATTEMPTS,
                    "reason": reason,
                    "next_action": "retry_lead",
                }),
            )?;
            tx.commit()?;
            return Ok(TransientHealOutcome::Exhausted(schedule));
        }

        let not_before = now + chrono::Duration::seconds(continuation_backoff_seconds(attempt));
        let state = TransientHealV1 {
            attempts: attempt,
            first_failed_at: rfc3339(first_failed_at),
            last_reason: reason.to_string(),
            owner_session_id: owner,
            exhausted: false,
            last_attempt_at: Some(rfc3339(now)),
            paused_at: None,
        };
        let state_json = serde_json::to_string(&state)
            .map_err(|error| crate::error::DaemonError::Store(error.to_string()))?;
        let message = format!(
            "Resuming after a transient failure (attempt {attempt}/{TRANSIENT_HEAL_MAX_ATTEMPTS}): {reason}."
        );
        let updated = tx.execute(
            "UPDATE scheduled_jobs
             SET enabled=1, next_fire_at=?2, updated_at=?3, message=?4, wake_session_id=?6,
                 schedule_json=json_remove(
                     json_set(schedule_json,'$.transient_heal',json(?5)),
                     '$.continuation_retry')
             WHERE id=?1 AND json_valid(schedule_json)",
            params![
                job_id.to_string(),
                rfc3339(not_before),
                rfc3339(now),
                message,
                state_json,
                session_id.to_string()
            ],
        )?;
        if updated == 0 {
            let job = ScheduledJob {
                id: job_id,
                name: format!("Transient heal {}", &session_id.to_string()[..8]),
                message,
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: not_before,
                },
                last_fired_at: None,
                next_fire_at: not_before,
                enabled: true,
                working_dir: None,
                provider: None,
                model: None,
                project_id: session.project_id,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(session_id),
            };
            super::scheduled_jobs::insert_scheduled_job_conn(&tx, &job)?;
            tx.execute(
                "UPDATE scheduled_jobs
                 SET schedule_json=json_set(schedule_json,'$.transient_heal',json(?2))
                 WHERE id=?1",
                params![job_id.to_string(), state_json],
            )?;
        }
        schedule.not_before = Some(not_before);
        schedule.manager_notified = self.record_manager_heal_notice(
            session_id,
            &format!("transient_heal_scheduled:{attempt}"),
            &json!({
                "attempt": attempt,
                "max_attempts": TRANSIENT_HEAL_MAX_ATTEMPTS,
                "not_before": rfc3339(not_before),
                "reason": reason,
            }),
        )?;
        tx.commit()?;
        Ok(TransientHealOutcome::Scheduled(schedule))
    }
}

#[cfg(test)]
#[path = "transient_heal_tests.rs"]
mod tests;
