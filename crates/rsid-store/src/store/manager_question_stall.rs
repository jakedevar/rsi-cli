//! Issue #1861: a manager seat (a project PM or a portfolio/area manager)
//! that ends its turn asking a question in chat stalls silently. Nobody reads
//! a chat ending, and a seat with no resume or `when` wake is never resumed.
//! Store half of the guard: the seat check, the one-shot safety wake and its
//! cap. The turn classifier and the lifecycle hook live in
//! `session::manager_question_stall`.

use rsi_common::types::Session;
use rusqlite::params;
use uuid::Uuid;

use crate::store::Store;

/// Carried by the safety-wake message. Its presence in the transcript is the
/// durable count of nudges this session has already received.
pub const MANAGER_QUESTION_MARKER: &str = "[rsid-manager-question]";

pub const MANAGER_QUESTION_WAKE_NAME_PREFIX: &str = "manager-question-stall-";

/// Nudges one session receives before the daemon only records friction.
pub const MANAGER_QUESTION_NUDGE_CAP: i64 = 3;

/// Seconds between the turn ending and the safety wake firing.
pub const MANAGER_QUESTION_WAKE_DELAY_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerQuestionArm {
    /// A one-shot resume wake was armed.
    Armed,
    /// The seat already holds an enabled resume/when wake: it will continue.
    HoldsResumeWake,
    /// The session already received [`MANAGER_QUESTION_NUDGE_CAP`] nudges.
    CapReached,
}

impl Store {
    /// The session holds a manager seat: a portfolio seat, a project's live
    /// PM or an active area node's seat.
    pub fn session_holds_manager_seat(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self.tier_caller_node(session_id)?.is_some())
    }

    /// Nudges already delivered or still armed for this session.
    fn manager_question_nudges(&self, session_id: Uuid) -> crate::error::Result<i64> {
        let delivered: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM conversation_events
              WHERE session_id=?1 AND role='User' AND instr(content, ?2)>0",
            params![session_id.to_string(), MANAGER_QUESTION_MARKER],
            |row| row.get(0),
        )?;
        let armed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM scheduled_jobs
              WHERE wake_session_id=?1 AND enabled=1 AND name LIKE ?2",
            params![
                session_id.to_string(),
                format!("{MANAGER_QUESTION_WAKE_NAME_PREFIX}%")
            ],
            |row| row.get(0),
        )?;
        Ok(delivered + armed)
    }

    /// Arm the one-shot safety wake for a manager seat that ended its turn
    /// asking for a decision, unless it already holds a resume/when wake or
    /// has used its nudge budget. The caller holds the store lock, so the
    /// check and the insert are one step.
    pub fn arm_manager_question_wake(
        &self,
        session: &Session,
    ) -> crate::error::Result<ManagerQuestionArm> {
        if self.session_has_enabled_resume_wake(session.id)? {
            return Ok(ManagerQuestionArm::HoldsResumeWake);
        }
        let nudges = self.manager_question_nudges(session.id)?;
        if nudges >= MANAGER_QUESTION_NUDGE_CAP {
            return Ok(ManagerQuestionArm::CapReached);
        }
        let message = rsi_common::daemon_message::wrap(
            "manager-question",
            &format!(
                "{MANAGER_QUESTION_MARKER} Your last turn ended asking a question in chat. Nobody \
                 reads a chat ending and you held no wake, so the daemon resumed you. Decide the \
                 engineering question yourself and act. If it is a real operator gate (main or \
                 a release, real spending, credentials, deleting user data), send it up with \
                 AgentReportUp and arm a `resume` wake before you end the turn. Never end a turn \
                 on a question."
            ),
        );
        let wake = crate::store_support::schedule_wake_job::build_agent_scheduled_job(
            crate::store_support::schedule_wake_job::ScheduleWakeRequest {
                message,
                in_seconds: Some(MANAGER_QUESTION_WAKE_DELAY_SECS),
                at: None,
                name: Some(format!(
                    "{MANAGER_QUESTION_WAKE_NAME_PREFIX}{}-{}",
                    session.id,
                    nudges + 1
                )),
                every_seconds: None,
                mode: Some("resume".to_string()),
                working_dir: session
                    .sandbox_root
                    .clone()
                    .unwrap_or_else(|| session.working_dir.clone()),
                provider: Some(session.provider),
                model: session.model.clone(),
                project_id: session.project_id,
                origin_session_id: Some(session.id),
                watch_session_id: None,
            },
        )
        .map_err(crate::error::DaemonError::InvalidParam)?;
        self.insert_scheduled_job(&wake)?;
        Ok(ManagerQuestionArm::Armed)
    }
}
