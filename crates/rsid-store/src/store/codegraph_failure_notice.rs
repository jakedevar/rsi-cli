//! #2454: tell the workspace owner about a codegraph failure episode when it
//! starts, not after the day-old project health Issue (#2246).
//!
//! An *episode* is one unhealthy stretch of a workspace's graph: it begins at
//! the first real failed run after a good state and ends at the next success.
//! [`Store::record_codegraph_failure_notice`] routes one evidence-bearing notice
//! per `(workspace, episode)` to whoever can act:
//!
//! - the owning session's agent mail, for an RSI sandbox checkout whose owner is
//!   still live (the checkout is theirs and the project graph does not cover it);
//! - otherwise a manager-inbox notice (`ledger_change`, subject
//!   `codegraph_failure:<workspace>`, version = the episode start) when the
//!   project has a live appointed manager;
//! - otherwise nothing (the day-old Issue remains the backstop).
//!
//! Decisions:
//! - The notice is independent of the project health Issue. Closing that Issue
//!   acknowledges the episode it described; a later episode is a new failure and
//!   gets its own notice. The Issue itself is never reopened or touched here.
//! - Every route is idempotent on the episode, so a retry, a repeated sweep or a
//!   restarted daemon that sees the same episode records nothing new. Failures
//!   inside one episode (even with a changed error) add no further notice.

use super::Store;
use crate::error::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_coordination::AgentSendMessageRequestV1;
use rsi_common::types::SessionStatus;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

/// `record_kind` of the notice.
pub const CODEGRAPH_FAILURE_NOTICE: &str = "codegraph_failure";
/// Longest error text carried in a notice.
const MAX_ERROR_CHARS: usize = 600;

/// One failure episode of one workspace, as the index manager saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodegraphFailureReport {
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    /// `primary`, `sandbox`, `worktree` or `detached`.
    pub checkout: String,
    /// The sandbox custody id of an RSI sandbox checkout.
    pub sandbox_custody_id: Option<Uuid>,
    /// `degraded` or `failed`.
    pub phase: String,
    /// First real failed run of the episode.
    pub episode_started_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub failed_runs: u64,
}

/// Where the notice went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodegraphFailureRoute {
    /// New agent mail to the owning session.
    OwnerMail(Uuid),
    /// New manager-inbox notice (the manager's watch job).
    ManagerNotice(Uuid),
    /// This episode was already recorded; nothing new was written.
    AlreadyRecorded,
    /// No live owner or manager to tell.
    Unrouted,
}

impl CodegraphFailureReport {
    /// The identity of the episode the notice is deduplicated on.
    #[must_use]
    pub fn episode_key(&self) -> String {
        self.episode_started_at
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    fn error_text(&self) -> String {
        let error = self.last_error.as_deref().unwrap_or("none recorded");
        error.chars().take(MAX_ERROR_CHARS).collect()
    }

    /// The typed record the notice or mail carries.
    #[must_use]
    pub fn state(&self) -> Value {
        json!({
            "record_kind": CODEGRAPH_FAILURE_NOTICE,
            "project_id": self.project_id,
            "workspace_id": self.workspace_id,
            "checkout": self.checkout,
            "phase": self.phase,
            "episode_started_at": self.episode_key(),
            "failed_runs": self.failed_runs,
            "last_error": self.error_text(),
            "detail": self.detail(),
        })
    }

    fn detail(&self) -> String {
        format!(
            "[rsi daemon, #2454 codegraph_failure] The codegraph index of the {checkout} checkout \
             (workspace {workspace}, project {project}) is {phase} since {since}: {runs} failed \
             run(s), last error `{error}`. The daemon retries an unchanged failure on a growing \
             delay (up to 6 hours) and files the project health Issue only after 24 hours; this \
             is the first signal of this episode and the only one until the graph recovers and \
             fails again. Live state: GetCodegraphStatus; fix the cause or record why it is \
             expected.",
            checkout = self.checkout,
            workspace = self.workspace_id,
            project = self.project_id,
            phase = self.phase,
            since = self.episode_key(),
            runs = self.failed_runs,
            error = self.error_text(),
        )
    }
}

impl Store {
    /// Route one failure-episode notice (see the module docs). Idempotent per
    /// `(workspace, episode)`.
    ///
    /// # Errors
    /// A persistence error; the caller retries on its next sweep.
    pub fn record_codegraph_failure_notice(
        &self,
        report: &CodegraphFailureReport,
    ) -> Result<CodegraphFailureRoute> {
        if let Some(owner) = self.live_codegraph_owner(report)? {
            match self.send_codegraph_owner_mail(owner, report) {
                Ok(route) => return Ok(route),
                // A full queue or a changed target falls through to the manager.
                Err(error) => tracing::debug!(
                    %owner,
                    %error,
                    "codegraph failure owner mail refused; trying the manager"
                ),
            }
        }
        self.record_codegraph_failure_manager_notice(report)
    }

    /// The live owning session of a sandbox checkout, when there is one.
    fn live_codegraph_owner(&self, report: &CodegraphFailureReport) -> Result<Option<Uuid>> {
        let Some(custody_id) = report.sandbox_custody_id else {
            return Ok(None);
        };
        let owner: Option<String> = self
            .conn
            .query_row(
                "SELECT owner_session_id FROM sandbox_custody_roots
                 WHERE custody_id=?1 AND state='live'",
                [custody_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(owner) = owner.and_then(|id| Uuid::parse_str(&id).ok()) else {
            return Ok(None);
        };
        let live = self.get_session(owner)?.is_some_and(|session| {
            matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            )
        });
        Ok(live.then_some(owner))
    }

    fn send_codegraph_owner_mail(
        &self,
        owner: Uuid,
        report: &CodegraphFailureReport,
    ) -> Result<CodegraphFailureRoute> {
        let idempotency_key = format!(
            "codegraph-failure:{}:{}",
            report.workspace_id,
            report.episode_key()
        );
        // The text carries the run count and latest error, which grow within an
        // episode; dedupe on the key, not on the replay fingerprint.
        if self.agent_message_key_accepted(owner, &idempotency_key)? {
            return Ok(CodegraphFailureRoute::AlreadyRecorded);
        }
        let outcome = self.accept_agent_message(
            owner,
            None,
            &AgentSendMessageRequestV1 {
                target_session_id: owner,
                message: report.detail(),
                idempotency_key,
                // Default expiry: a fixed `expires_at` would differ per call and
                // make a replay an idempotency conflict.
                expires_at: None,
            },
        )?;
        Ok(match outcome {
            super::agent_coordination::AcceptAgentMessageOutcome::Accepted(receipt) => {
                CodegraphFailureRoute::OwnerMail(receipt.message_id)
            }
            super::agent_coordination::AcceptAgentMessageOutcome::Replayed(_) => {
                CodegraphFailureRoute::AlreadyRecorded
            }
        })
    }

    fn record_codegraph_failure_manager_notice(
        &self,
        report: &CodegraphFailureReport,
    ) -> Result<CodegraphFailureRoute> {
        let project_id = report.project_id;
        let Some(config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(CodegraphFailureRoute::Unrouted);
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self
                .get_harness_manager_policy(project_id)?
                .is_none_or(|policy| policy.revoked)
        {
            return Ok(CodegraphFailureRoute::Unrouted);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (job_id, _, _) = self.manager_action_watch_identity(&config);
        let subject = format!("{CODEGRAPH_FAILURE_NOTICE}:{}", report.workspace_id);
        let version = report.episode_key();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject, version],
            |row| row.get(0),
        )?;
        if exists {
            tx.commit()?;
            return Ok(CodegraphFailureRoute::AlreadyRecorded);
        }
        self.ensure_manager_action_watch(&config, &format!("{subject}:{version}"))?; // sql-dynamic-ok: watch signature, not SQL
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
                serde_json::to_string(&report.state())?,
                super::harness_manager_v2::now(),
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        tx.commit()?;
        Ok(CodegraphFailureRoute::ManagerNotice(job_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_coordinator::tests::fixture;
    use crate::test_support::make_test_session;
    use chrono::Duration;
    use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};

    fn policy() -> ManagerPolicyV2 {
        ManagerPolicyV2 {
            capabilities: vec![ManagerCapabilityV2::WorkPlan],
            ..Default::default()
        }
    }

    fn report(
        project_id: Uuid,
        checkout: &str,
        custody: Option<Uuid>,
        started: DateTime<Utc>,
        error: &str,
    ) -> CodegraphFailureReport {
        CodegraphFailureReport {
            project_id,
            workspace_id: Uuid::from_u128(7),
            checkout: checkout.into(),
            sandbox_custody_id: custody,
            phase: "failed".into(),
            episode_started_at: started,
            last_error: Some(error.into()),
            failed_runs: 1,
        }
    }

    fn manager_notices(store: &Store, subject: &str) -> Vec<(String, String)> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT subject_version,state_json FROM harness_manager_notices
                 WHERE kind='ledger_change' AND subject_id=?1 ORDER BY sequence",
            )
            .unwrap();
        stmt.query_map([subject], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn primary_episode_notifies_the_manager_once_and_again_after_recovery_2454() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy());
        let first = Utc::now() - Duration::minutes(5);
        let one = report(
            config.project_id,
            "primary",
            None,
            first,
            "source path is a symlink",
        );

        let route = store.record_codegraph_failure_notice(&one).unwrap();
        assert!(
            matches!(route, CodegraphFailureRoute::ManagerNotice(_)),
            "{route:?}"
        );
        // Retries and a changed error in the same episode add nothing.
        assert_eq!(
            store.record_codegraph_failure_notice(&one).unwrap(),
            CodegraphFailureRoute::AlreadyRecorded
        );
        let mut same_episode = one.clone();
        same_episode.last_error = Some("codegraph source changed during indexing".into());
        same_episode.failed_runs = 9;
        assert_eq!(
            store
                .record_codegraph_failure_notice(&same_episode)
                .unwrap(),
            CodegraphFailureRoute::AlreadyRecorded
        );
        let subject = format!("{CODEGRAPH_FAILURE_NOTICE}:{}", one.workspace_id);
        let rows = manager_notices(&store, &subject);
        assert_eq!(rows.len(), 1);
        let state: Value = serde_json::from_str(&rows[0].1).unwrap();
        assert_eq!(state["record_kind"], CODEGRAPH_FAILURE_NOTICE);
        assert_eq!(state["checkout"], "primary");
        assert_eq!(state["last_error"], "source path is a symlink");
        assert_eq!(state["workspace_id"], one.workspace_id.to_string());

        // The graph recovered and failed again: a new episode, a new notice.
        let later = report(
            config.project_id,
            "primary",
            None,
            Utc::now(),
            "parser exhausted its budget",
        );
        let route = store.record_codegraph_failure_notice(&later).unwrap();
        assert!(
            matches!(route, CodegraphFailureRoute::ManagerNotice(_)),
            "{route:?}"
        );
        assert_eq!(manager_notices(&store, &subject).len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn sandbox_episode_mails_its_live_owner_once_2454() {
        let store = Store::open_in_memory().unwrap();
        let mut session = make_test_session();
        session.status = SessionStatus::Running;
        store.insert_session(&session).unwrap();
        let custody = Uuid::new_v4();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO sandbox_custody_roots
                 (custody_id,allocation_id,canonical_repo_dir,sandbox_root,sandbox_branch,
                  repository_identity,source_commit,state,owner_session_id,generation,
                  event_sequence,validation_state,created_at,updated_at)
                 VALUES (?1,?1,'/tmp/repo','/tmp/box','rsi/test','repo',
                         '0000000000000000000000000000000000000000',
                         'live',?2,1,1,'unverified',?3,?3)",
                params![custody.to_string(), session.id.to_string(), now],
            )
            .unwrap();
        let project = session.project_id.unwrap();
        let started = Utc::now();
        let one = report(
            project,
            "sandbox",
            Some(custody),
            started,
            "index worker stopped",
        );

        let route = store.record_codegraph_failure_notice(&one).unwrap();
        let CodegraphFailureRoute::OwnerMail(message) = route else {
            panic!("expected owner mail, got {route:?}");
        };
        assert_eq!(
            store.record_codegraph_failure_notice(&one).unwrap(),
            CodegraphFailureRoute::AlreadyRecorded
        );
        let mut grown = one.clone();
        grown.failed_runs = 12;
        grown.last_error = Some("a different error".into());
        assert_eq!(
            store.record_codegraph_failure_notice(&grown).unwrap(),
            CodegraphFailureRoute::AlreadyRecorded,
            "a later retry of the same episode never mails twice"
        );
        let (target, payload): (String, String) = store
            .conn
            .query_row(
                "SELECT target_session_id,payload FROM agent_messages WHERE id=?1",
                [message.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(target, session.id.to_string());
        assert!(payload.contains("index worker stopped"), "{payload}");
        assert!(payload.contains(&one.workspace_id.to_string()), "{payload}");

        // A later episode of the same sandbox is a new mail.
        let again = report(
            project,
            "sandbox",
            Some(custody),
            started + Duration::hours(1),
            "x",
        );
        assert!(matches!(
            store.record_codegraph_failure_notice(&again).unwrap(),
            CodegraphFailureRoute::OwnerMail(_)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn episode_without_owner_or_manager_is_unrouted_2454() {
        let store = Store::open_in_memory().unwrap();
        let one = report(
            Uuid::new_v4(),
            "sandbox",
            Some(Uuid::new_v4()),
            Utc::now(),
            "e",
        );
        assert_eq!(
            store.record_codegraph_failure_notice(&one).unwrap(),
            CodegraphFailureRoute::Unrouted
        );
    }
}
