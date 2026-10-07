//! #1337: tell the owning manager about a runaway agent process tree.
//!
//! The daemon's CPU-time andon (`rsid::cpu_andon`) samples each agent-owned
//! process tree (a session's provider scope or a job unit). When one passes
//! the operator's CPU-minute threshold, or dominates a loaded host, it records
//! a `runaway_process` friction event and calls
//! [`Store::record_runaway_process_notice`], which routes one typed record to
//! whoever can stop it:
//!
//! - a manager-inbox notice (`ledger_change`, subject
//!   `runaway_process:<session>`, version = the tree id) when the session's
//!   project has a live appointed manager;
//! - otherwise a durable agent mail to the session's Epic lead;
//! - otherwise nothing (the friction event and the operator's system message
//!   remain).
//!
//! Each route is idempotent on the tree: a restarted daemon that samples the
//! same tree again records nothing new. The daemon never stops the tree
//! itself; the record carries the suggested halt.

use super::Store;
use super::worker_baton::WorkerLauncherKind;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_coordination::AgentSendMessageRequestV1;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

/// `record_kind` of the notice and the subject prefix.
pub const RUNAWAY_PROCESS_NOTICE: &str = "runaway_process";
/// How long the fallback mail to an Epic lead stays deliverable.
const RUNAWAY_MAIL_EXPIRY_HOURS: i64 = 6;

/// One runaway tree, as measured by the sampler.
#[derive(Debug, Clone, PartialEq)]
pub struct RunawayProcessReport {
    /// The agent session that owns the tree.
    pub session_id: Uuid,
    /// `session` (a provider scope) or `job_<kind>` (a job unit).
    pub tree: String,
    /// The model invocation (session scope) or job id.
    pub tree_id: Uuid,
    /// The systemd unit the measurement came from.
    pub unit: String,
    /// `cpu_minutes` or `host_load`.
    pub reason: String,
    pub cpu_minutes: f64,
    /// Cores used over the last sample window, when two samples exist.
    pub cores: Option<f64>,
    pub host_load: Option<f64>,
    /// The operator threshold that tripped.
    pub threshold: u32,
    pub observed_at: DateTime<Utc>,
}

/// Where the record went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunawayNoticeRoute {
    /// The manager-inbox notice job (new or already recorded for this tree).
    ManagerNotice(Uuid),
    /// The agent mail to the Epic lead.
    LeadMail(Uuid),
    /// No manager or lead to tell.
    Unrouted,
}

impl RunawayProcessReport {
    /// The one-call halt the record suggests.
    #[must_use]
    pub fn suggested_action(&self) -> String {
        let halt = format!(
            "AgentHalt {{\"session_id\":\"{}\"}}, then AgentContinueChild with a scoped instruction (filters from scripts/check-touched-shards; scripts/scoped-test)",
            self.session_id
        );
        if self.tree == "session" {
            halt
        } else {
            format!(
                "the owner stops job {} with AgentCancelJob, or {halt}",
                self.tree_id
            )
        }
    }

    /// The typed record carried by the notice or mail.
    #[must_use]
    pub fn state(&self) -> Value {
        json!({
            "record_kind": RUNAWAY_PROCESS_NOTICE,
            "session_id": self.session_id,
            "tree": self.tree,
            "tree_id": self.tree_id,
            "unit": self.unit,
            "reason": self.reason,
            "cpu_minutes": (self.cpu_minutes * 10.0).round() / 10.0,
            "cores": self.cores.map(|cores| (cores * 10.0).round() / 10.0),
            "host_load": self.host_load,
            "threshold": self.threshold,
            "observed_at": self.observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "suggested": self.suggested_action(),
        })
    }
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|error| DaemonError::Store(format!("invalid stored id: {error}")))
}

impl Store {
    /// Route one runaway-tree record to the owning manager (see the module
    /// docs). Idempotent per tree.
    ///
    /// # Errors
    /// A persistence error; the caller treats the andon as best-effort.
    pub fn record_runaway_process_notice(
        &self,
        report: &RunawayProcessReport,
    ) -> Result<RunawayNoticeRoute> {
        let Some(session) = self.get_session(report.session_id)? else {
            return Ok(RunawayNoticeRoute::Unrouted);
        };
        if let Some(project) = session.project_id
            && let Some(job) = self.record_runaway_manager_notice(project, report)?
        {
            return Ok(RunawayNoticeRoute::ManagerNotice(job));
        }
        let Some(launcher) = self.worker_baton_launcher(report.session_id)? else {
            return Ok(RunawayNoticeRoute::Unrouted);
        };
        if launcher.kind != WorkerLauncherKind::EpicLead || launcher.launcher == report.session_id {
            return Ok(RunawayNoticeRoute::Unrouted);
        }
        let mail = self.accept_agent_message(
            report.session_id,
            None,
            &AgentSendMessageRequestV1 {
                target_session_id: launcher.launcher,
                message: format!(
                    "[rsi daemon, #1337 runaway_process] {}\nStop it now unless it is about to finish: {}. Report the stopped run in your handoff.",
                    report.state(),
                    report.suggested_action()
                ),
                idempotency_key: format!("runaway-process:{}", report.tree_id),
                expires_at: Some(
                    report.observed_at + chrono::Duration::hours(RUNAWAY_MAIL_EXPIRY_HOURS),
                ),
            },
        )?;
        Ok(RunawayNoticeRoute::LeadMail(mail.receipt().message_id))
    }

    /// The manager-inbox notice, when the project has a live appointed
    /// manager. `None` otherwise.
    fn record_runaway_manager_notice(
        &self,
        project: Uuid,
        report: &RunawayProcessReport,
    ) -> Result<Option<Uuid>> {
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none() || config.is_revoked() {
            return Ok(None);
        }
        let subject = format!("{RUNAWAY_PROCESS_NOTICE}:{}", report.session_id);
        let version = report.tree_id.to_string();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT job_id FROM harness_manager_notices
                 WHERE kind='ledger_change' AND subject_id=?1 AND subject_version=?2
                 ORDER BY sequence LIMIT 1",
                params![subject, version],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(job) = existing {
            tx.commit()?;
            return Ok(Some(parse_uuid(&job)?));
        }
        let (job_id, _, _) = self.manager_action_watch_identity(&config);
        self.ensure_manager_action_watch(&config, &format!("{subject}:{version}"))?; // sql-dynamic-ok: watch signature, not SQL
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',?6,?4,'ledger_change',?7,?8,?9,?10,?10)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                project.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                report.session_id.to_string(),
                subject,
                version,
                serde_json::to_string(&report.state())?,
                super::harness_manager_v2::now(),
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        tx.commit()?;
        Ok(Some(job_id))
    }

    /// The session that launched model invocation `invocation`, when recorded.
    ///
    /// # Errors
    /// A persistence error.
    pub fn model_invocation_session(&self, invocation: Uuid) -> Result<Option<Uuid>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT session_id FROM model_invocations WHERE id=?1",
                [invocation.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        raw.flatten().map(|id| parse_uuid(&id)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(session: Uuid, tree: &str) -> RunawayProcessReport {
        RunawayProcessReport {
            session_id: session,
            tree: tree.into(),
            tree_id: Uuid::from_u128(7),
            unit: "rsi-job-x.service".into(),
            reason: "cpu_minutes".into(),
            cpu_minutes: 812.345,
            cores: Some(13.26),
            host_load: Some(71.5),
            threshold: 240,
            observed_at: Utc::now(),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runaway_record_names_the_session_and_a_one_call_halt() {
        let session = Uuid::from_u128(42);
        let state = report(session, "session").state();
        assert_eq!(state["record_kind"], RUNAWAY_PROCESS_NOTICE);
        assert_eq!(state["session_id"], session.to_string());
        assert_eq!(state["cpu_minutes"], 812.3);
        assert_eq!(state["cores"], 13.3);
        let suggested = state["suggested"].as_str().unwrap();
        assert!(suggested.starts_with(&format!("AgentHalt {{\"session_id\":\"{session}\"}}")));
        assert!(suggested.contains("AgentContinueChild"));
        let job = report(session, "job_test").suggested_action();
        assert!(job.contains("AgentCancelJob"), "{job}");
        assert!(job.contains(&Uuid::from_u128(7).to_string()), "{job}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn runaway_notice_for_an_unknown_session_is_unrouted() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store
                .record_runaway_process_notice(&report(Uuid::new_v4(), "session"))
                .unwrap(),
            RunawayNoticeRoute::Unrouted
        );
        assert_eq!(
            store.model_invocation_session(Uuid::new_v4()).unwrap(),
            None
        );
    }
}
