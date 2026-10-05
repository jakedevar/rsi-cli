//! Store half of the worker no-result guard (issue #1098): the wake identity
//! constants and the `Store` queries the session-side guard calls. Moved here
//! from `session::worker_result_guard` so `store` has no edge into `session`
//! (issue #1021 S4); the guard re-exports the constants at their old path.

use rsi_common::types::{Session, SessionStatus};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::store::Store;

/// Marker carried by the automatic continuation message. Its presence in the
/// transcript is the durable "already nudged once" record.
pub const NO_RESULT_MARKER: &str = "[rsid-no-result]";

const NO_RESULT_NAMESPACE: Uuid = Uuid::from_u128(0x1098_0000_0000_4000_8000_0000_0000_0001);

/// Typed refusal: the delivery-time re-proof found the worker ineligible and
/// the wake is retired (never deferred, never delivered).
pub const NO_RESULT_RETIRED: &str = "no_result_continuation_retired";

pub const NO_RESULT_WAKE_NAME_PREFIX: &str = "worker-no-result-";

/// Deterministic id of a worker's single automatic continuation wake.
pub fn no_result_wake_id(session_id: Uuid) -> Uuid {
    Uuid::new_v5(&NO_RESULT_NAMESPACE, session_id.as_bytes())
}

impl Store {
    /// Covered worker: a leaf with a parent that is neither a manager seat nor
    /// an Epic lead.
    pub fn worker_no_result_covered(&self, session: &Session) -> crate::error::Result<bool> {
        let Some(parent) = session.parent_id else {
            return Ok(false);
        };
        if !rsi_common::is_leaf_kind(session.session_kind) {
            return Ok(false);
        }
        if self.get_session(parent)?.is_none() {
            return Ok(false);
        }
        let leads_epic: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE lead_session_id=?1)",
            params![session.id.to_string()],
            |row| row.get(0),
        )?;
        if leads_epic {
            return Ok(false);
        }
        if let Some(project) = session.project_id
            && self
                .get_harness_manager_notice_config(project)?
                .and_then(|config| config.current_session_id)
                == Some(session.id)
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// The single automatic continuation was already issued for this worker.
    pub fn worker_no_result_nudged(&self, session_id: Uuid) -> crate::error::Result<bool> {
        let wake: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id=?1)",
            params![no_result_wake_id(session_id).to_string()],
            |row| row.get(0),
        )?;
        if wake {
            return Ok(true);
        }
        let marker: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_events
              WHERE session_id=?1 AND role='User' AND instr(content, ?2)>0)",
            params![session_id.to_string(), NO_RESULT_MARKER],
            |row| row.get(0),
        )?;
        Ok(marker)
    }

    /// #1109: the worker's automatic continuation wake is still armed, so its
    /// Completed status is transient and a terminal watch must not fire yet.
    pub fn no_result_wake_pending(&self, session_id: Uuid) -> crate::error::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id=?1 AND enabled=1)",
            params![no_result_wake_id(session_id).to_string()],
            |row| row.get(0),
        )?)
    }

    /// #1124: delivery-time re-proof for a no-result continuation wake, run
    /// under the target's spawn guard immediately before the provider effect.
    /// Jobs that are not no-result wakes pass untouched. The wake is only ever
    /// delivered to its own Completed leaf worker that nobody halted, paused
    /// or archived and that has no pending question or unresolved approval;
    /// anything else is refused with `NO_RESULT_RETIRED` and the scheduler
    /// retires the row.
    pub(crate) fn check_no_result_wake_delivery(
        &self,
        target: Uuid,
        job_ids: &[Uuid],
    ) -> crate::error::Result<()> {
        use crate::error::DaemonError;
        let retired = || DaemonError::InvalidParam(NO_RESULT_RETIRED.into());
        for job in job_ids {
            let name: Option<String> = self
                .conn
                .query_row(
                    "SELECT name FROM scheduled_jobs WHERE id=?1",
                    params![job.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(worker) = name
                .as_deref()
                .and_then(|name| name.strip_prefix(NO_RESULT_WAKE_NAME_PREFIX))
                .and_then(|id| Uuid::parse_str(id).ok())
                .filter(|worker| no_result_wake_id(*worker) == *job)
            else {
                continue;
            };
            if worker != target {
                return Err(retired());
            }
            let Some(session) = self.get_session(worker)? else {
                return Err(retired());
            };
            if session.status != SessionStatus::Completed
                || session.pending_archive
                || !self.worker_no_result_covered(&session)?
                || self.get_operator_pause(worker)?
                    != crate::store::manager_actions::OperatorPause::None
            {
                return Err(retired());
            }
            // Raw markers: hydration tolerates malformed JSON, which must not
            // erase a human gate.
            let held: bool = self.conn.query_row(
                "SELECT pending_question_json IS NOT NULL
                     OR EXISTS(SELECT 1 FROM approvals WHERE session_id=?1 AND status='Pending')
                 FROM sessions WHERE id=?1",
                params![worker.to_string()],
                |row| row.get(0),
            )?;
            if held {
                return Err(retired());
            }
        }
        Ok(())
    }

    /// A daemon job the session owns is still running.
    pub fn owner_has_running_agent_job(&self, owner: Uuid) -> crate::error::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM agent_jobs WHERE owner_session_id=?1 AND state='running' LIMIT 1",
                params![owner.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}
