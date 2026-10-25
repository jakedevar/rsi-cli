//! The one manager notice an operator intake seat can cause (#3787).
//!
//! An intake seat files Issues; only a P1 (or an explicit `notify-manager`
//! label) mails the manager, once per Issue, naming it. Everything else
//! appears in the manager's normal Issue scan. The subject is the Issue and
//! the version is fixed, so a replayed filing never notifies twice, and the
//! notice carries over a seat succession like every project-wide notice.

use super::Store;
use crate::error::Result;
use rusqlite::params;
use serde_json::json;
use uuid::Uuid;

/// Subject prefix of the notice; the Issue's display number follows.
pub const OPERATOR_INTAKE_SUBJECT_PREFIX: &str = "operator_intake:";
/// `record_kind` of the notice state.
const OPERATOR_INTAKE: &str = "operator_intake";

/// What the notice tells the manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorIntakeNotice {
    pub issue_number: i64,
    pub title: String,
    pub priority: u8,
    pub labels: Vec<String>,
}

impl Store {
    /// Record the notice for one intake-filed Issue. Returns the notice job to
    /// wake, or `None` when the project has no live manager seat or the Issue
    /// was already announced.
    ///
    /// # Errors
    /// A store failure.
    pub fn record_operator_intake_notice(
        &self,
        project_id: Uuid,
        notice: &OperatorIntakeNotice,
    ) -> Result<Option<Uuid>> {
        let Some(config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self
                .get_harness_manager_policy(project_id)?
                .is_none_or(|policy| policy.revoked)
        {
            return Ok(None);
        }
        let (job_id, _, _) = self.manager_action_watch_identity(&config);
        let subject = format!("{OPERATOR_INTAKE_SUBJECT_PREFIX}{}", notice.issue_number);
        let version = "1";
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject, version],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(None);
        }
        self.ensure_manager_action_watch(&config, version)?;
        let recorded_at = super::harness_manager_v2::now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,'ledger_change',?6,?7,?8,?9,?9)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                subject,
                version,
                serde_json::to_string(&json!({
                    "record_kind": OPERATOR_INTAKE,
                    "issue_number": notice.issue_number,
                    "title": notice.title,
                    "priority": notice.priority,
                    "labels": notice.labels,
                    "detail": format!(
                        "The operator intake agent filed Issue #{} (P{}) and flagged it for you \
                         now: {}. Read #{} and decide who works it; every other intake item \
                         waits in your normal Issue scan.",
                        notice.issue_number, notice.priority, notice.title, notice.issue_number
                    ),
                }))?,
                recorded_at,
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        Ok(Some(job_id))
    }
}
