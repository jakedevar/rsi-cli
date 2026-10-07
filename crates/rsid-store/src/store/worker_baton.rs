//! #1254: a worker passes the baton when its context fills.
//!
//! When the measured live context of a session that holds no coordinating
//! seat crosses the operator's worker cap (`worker_context_cap_tokens`), and
//! the session has a launching manager or Epic lead, the daemon:
//!
//! - (a) queues one durable agent mail to the worker with the baton protocol
//!   (commit, append a handoff to its Issue, end the turn with
//!   `PIPELINE HANDOFF — BATON <sha>` and a closing `Friction:` line, #1332),
//!   delivered at its next tool boundary;
//! - (b) records one typed `worker_context_cap` notice for the launcher: a
//!   manager-inbox notice when the launcher is the project's manager lineage,
//!   otherwise (an Epic lead, or a global seat acting in a project) a durable
//!   agent mail to the launcher carrying the same typed record.
//!
//! The crossing is one durable record (`worker_baton:<worker>` in
//! `daemon_settings`, kept, never deleted) written before either effect.
//! Each effect is idempotent on its own identity (the mail's
//! `(owner, idempotency key)`, the notice's `(kind, subject)`), and the record
//! flips to `sent` only after both landed, so a daemon that restarts in
//! between finishes the same effects once ([`Store::pending_worker_batons`]).
//! Nothing here interrupts a turn: the worker ends its own turn.

use super::Store;
use super::harness_manager_v2::refused;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_coordination::AgentSendMessageRequestV1;
use rsi_common::types::SessionKind;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

/// `daemon_settings` key prefix of a worker's baton record.
const RECORD_PREFIX: &str = "worker_baton:";
/// The typed notice's record kind.
pub const WORKER_CONTEXT_CAP_NOTICE: &str = "worker_context_cap";
/// Manager-notice subject prefix; the subject is `worker_context_cap:<worker>`.
pub const WORKER_CONTEXT_CAP_SUBJECT_PREFIX: &str = "worker_context_cap:";
/// How long the baton mail stays deliverable after the crossing.
const BATON_MAIL_EXPIRY_HOURS: i64 = 4;

/// Who launched a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLauncherKind {
    /// A manager seat, through `AgentManagerLaunchIssueWorker`.
    Manager,
    /// The lead of the worker's parent Epic.
    EpicLead,
}

/// The worker's launcher and the Issue it is bound to, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerBatonLauncher {
    pub launcher: Uuid,
    pub kind: WorkerLauncherKind,
    pub project_id: Option<Uuid>,
    pub issue_id: Option<Uuid>,
    pub issue_display_number: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerBatonState {
    /// Crossed; the mail and the notice may not both have landed yet.
    Due,
    /// Both effects landed.
    Sent,
}

/// One durable baton per worker. Rows are kept, never deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerBatonRecord {
    pub state: WorkerBatonState,
    #[serde(flatten)]
    pub launcher: WorkerBatonLauncher,
    pub measured_tokens: u64,
    pub cap_tokens: u64,
    pub crossed_at: DateTime<Utc>,
    #[serde(default)]
    pub worker_mail_id: Option<Uuid>,
    /// `manager_notice:<job>` or `mail:<message>`.
    #[serde(default)]
    pub launcher_notice: Option<String>,
}

/// What one crossing did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerBatonOutcome {
    /// The worker has neither a launching manager nor an Epic lead.
    NoLauncher,
    /// This call recorded the crossing and sent the baton.
    Sent,
    /// An earlier crossing of this worker already holds the baton record.
    AlreadyRecorded,
}

fn record_key(worker: Uuid) -> String {
    format!("{RECORD_PREFIX}{worker}")
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| refused("manager_v2_invalid_stored_identity"))
}

impl Store {
    /// The launcher of `worker`: the manager whose Issue-bound launch created
    /// it, else the lead of its parent Epic. `None` for anything else (an
    /// operator session, a child of a non-Epic, an Epic without a lead).
    /// #1325: the latest launch is by admission order, matching Issue bindings.
    pub fn worker_baton_launcher(&self, worker: Uuid) -> Result<Option<WorkerBatonLauncher>> {
        let bound: Option<(String, String, String, i64)> = self
            .conn
            .query_row(
                "SELECT json_extract(o.payload_json,'$.origin.caller'), o.project_id,
                        json_extract(o.payload_json,'$.issue_binding.issue_id'),
                        json_extract(o.payload_json,'$.issue_binding.display_number')
                 FROM harness_manager_v2_operations o
                 WHERE o.kind='lifecycle_action' AND o.target_session_id=?1
                   AND o.state IN ('running','succeeded')
                   AND json_extract(o.payload_json,'$.origin.origin')='agent'
                   AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL
                 ORDER BY o.rowid DESC LIMIT 1",
                [worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((caller, project, issue, display_number)) = bound {
            return Ok(Some(WorkerBatonLauncher {
                launcher: parse_uuid(&caller)?,
                kind: WorkerLauncherKind::Manager,
                project_id: Some(parse_uuid(&project)?),
                issue_id: Some(parse_uuid(&issue)?),
                issue_display_number: Some(display_number),
            }));
        }
        let Some(session) = self.get_session(worker)? else {
            return Ok(None);
        };
        let Some(parent) = session
            .parent_id
            .map(|id| self.get_session(id))
            .transpose()?
        else {
            return Ok(None);
        };
        let Some(parent) = parent else {
            return Ok(None);
        };
        Ok(match parent.lead_session_id {
            Some(lead) if parent.session_kind == SessionKind::Epic && lead != worker => {
                Some(WorkerBatonLauncher {
                    launcher: lead,
                    kind: WorkerLauncherKind::EpicLead,
                    project_id: session.project_id,
                    issue_id: None,
                    issue_display_number: None,
                })
            }
            _ => None,
        })
    }

    /// The worker's baton record, if it ever crossed its cap.
    pub fn worker_baton(&self, worker: Uuid) -> Result<Option<WorkerBatonRecord>> {
        self.get_daemon_setting(&record_key(worker))?
            .map(|raw| serde_json::from_str(&raw).map_err(|e| DaemonError::Store(e.to_string())))
            .transpose()
    }

    fn put_worker_baton(&self, worker: Uuid, record: &WorkerBatonRecord) -> Result<()> {
        self.set_daemon_setting(&record_key(worker), &serde_json::to_string(record)?)
    }

    /// Record one crossing of `worker`'s cap and pass the baton. A worker has
    /// at most one baton: a later crossing (after compaction, or after a
    /// restart cleared the in-memory latch) finds the record and does nothing
    /// new, except finishing a baton an earlier process left `due`.
    pub fn record_worker_context_cap(
        &self,
        worker: Uuid,
        measured_tokens: u64,
        cap_tokens: u64,
        now: DateTime<Utc>,
    ) -> Result<WorkerBatonOutcome> {
        if let Some(record) = self.worker_baton(worker)? {
            if record.state == WorkerBatonState::Due {
                self.deliver_worker_baton(worker)?;
            }
            return Ok(WorkerBatonOutcome::AlreadyRecorded);
        }
        let Some(launcher) = self.worker_baton_launcher(worker)? else {
            return Ok(WorkerBatonOutcome::NoLauncher);
        };
        self.put_worker_baton(
            worker,
            &WorkerBatonRecord {
                state: WorkerBatonState::Due,
                launcher,
                measured_tokens,
                cap_tokens,
                crossed_at: now,
                worker_mail_id: None,
                launcher_notice: None,
            },
        )?;
        self.deliver_worker_baton(worker)?;
        Ok(WorkerBatonOutcome::Sent)
    }

    /// Workers whose baton is recorded but not yet fully sent.
    pub fn pending_worker_batons(&self) -> Result<Vec<Uuid>> {
        let mut pending = Vec::new();
        for (key, raw) in self.list_daemon_settings_with_prefix(RECORD_PREFIX)? {
            let Ok(record) = serde_json::from_str::<WorkerBatonRecord>(&raw) else {
                continue;
            };
            if record.state == WorkerBatonState::Due
                && let Some(worker) = key
                    .strip_prefix(RECORD_PREFIX)
                    .and_then(|id| Uuid::parse_str(id).ok())
            {
                pending.push(worker);
            }
        }
        Ok(pending)
    }

    /// Send the baton mail and the launcher notice of a `due` record, each
    /// once, then mark it `sent`. Returns `false` when there was nothing due.
    pub fn deliver_worker_baton(&self, worker: Uuid) -> Result<bool> {
        let Some(mut record) = self.worker_baton(worker)? else {
            return Ok(false);
        };
        if record.state != WorkerBatonState::Due {
            return Ok(false);
        }
        if record.worker_mail_id.is_none() {
            let mail = self.accept_agent_message(
                record.launcher.launcher,
                None,
                &AgentSendMessageRequestV1 {
                    target_session_id: worker,
                    message: baton_mail(&record),
                    idempotency_key: format!("worker-baton:{worker}"),
                    expires_at: Some(
                        record.crossed_at + chrono::Duration::hours(BATON_MAIL_EXPIRY_HOURS),
                    ),
                },
            )?;
            record.worker_mail_id = Some(mail.receipt().message_id);
            self.put_worker_baton(worker, &record)?;
        }
        if record.launcher_notice.is_none() {
            let notice = match self.record_worker_cap_manager_notice(worker, &record)? {
                Some(job) => format!("manager_notice:{job}"),
                None => {
                    let mail = self.accept_agent_message(
                        worker,
                        None,
                        &AgentSendMessageRequestV1 {
                            target_session_id: record.launcher.launcher,
                            message: launcher_mail(worker, &record),
                            idempotency_key: format!("worker-baton-notice:{worker}"),
                            expires_at: Some(
                                record.crossed_at
                                    + chrono::Duration::hours(BATON_MAIL_EXPIRY_HOURS),
                            ),
                        },
                    )?;
                    format!("mail:{}", mail.receipt().message_id)
                }
            };
            record.launcher_notice = Some(notice);
        }
        record.state = WorkerBatonState::Sent;
        self.put_worker_baton(worker, &record)?;
        Ok(true)
    }

    /// The typed manager-inbox notice, when the launcher is the project's
    /// active manager lineage. `None` when the launcher must be told by mail.
    fn record_worker_cap_manager_notice(
        &self,
        worker: Uuid,
        record: &WorkerBatonRecord,
    ) -> Result<Option<Uuid>> {
        let (WorkerLauncherKind::Manager, Some(project)) =
            (record.launcher.kind, record.launcher.project_id)
        else {
            return Ok(None);
        };
        let Some(config) = self.get_harness_manager_notice_config(project)? else {
            return Ok(None);
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self.manager_lineage_root(record.launcher.launcher).ok()
                != Some(config.manager_session_id)
        {
            return Ok(None);
        }
        let subject = format!("{WORKER_CONTEXT_CAP_SUBJECT_PREFIX}{worker}");
        let version = record
            .crossed_at
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // The job identity follows the scope version; a notice recorded under
        // an earlier scope is still this crossing's one notice.
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
                worker.to_string(),
                subject,
                version,
                serde_json::to_string(&notice_state(worker, record))?,
                super::harness_manager_v2::now(),
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        tx.commit()?;
        Ok(Some(job_id))
    }
}

/// The typed `worker_context_cap` record.
fn notice_state(worker: Uuid, record: &WorkerBatonRecord) -> serde_json::Value {
    json!({
        "record_kind": WORKER_CONTEXT_CAP_NOTICE,
        "worker": worker,
        "issue": record.launcher.issue_id,
        "issue_display_number": record.launcher.issue_display_number,
        "measured_tokens": record.measured_tokens,
        "cap": record.cap_tokens,
        "crossed_at": record.crossed_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
    })
}

fn baton_mail(record: &WorkerBatonRecord) -> String {
    let handoff = record.launcher.issue_display_number.map_or_else(
        || "Write the handoff (what is done, what is left, the commit SHA, test state, the next step) in your final message.".to_string(),
        |issue| format!(
            "Append a handoff to Issue #{issue} with AgentUpdateIssue (read it with AgentGetIssue, then send only body: the current body unchanged followed by a section headed `## Baton handoff` with what is done, what is left, the commit SHA, test state, the next step and your `Friction:` line, and expected_row_version from that read)."
        ),
    );
    format!(
        "[rsi daemon, #1254 baton] Your measured context is {} tokens, past your worker cap of {} tokens. Pass the baton now and start no new work:\n1. Commit your work on your sandbox branch (WIP is fine).\n2. {handoff}\n3. End your turn with a final message whose first line is `PIPELINE HANDOFF — BATON <sha>` and whose last line is `Friction: none | #N[, #M] | <one line, not filed because ...>` (the kaizen Issues you filed, or the problem you could not file).\nYour launcher was told and will continue from your commit with a fresh worker.",
        record.measured_tokens, record.cap_tokens
    )
}

fn launcher_mail(worker: Uuid, record: &WorkerBatonRecord) -> String {
    format!(
        "[rsi daemon, #1254 worker_context_cap] {}\nWorker {worker} crossed its context cap and was told to commit, append a handoff and end its turn. When it has ended its turn, continue its Issue with a fresh worker (AgentManagerLaunchIssueWorker with continue_from: \"{worker}\", or a new child from its commit).",
        notice_state(worker, record)
    )
}
