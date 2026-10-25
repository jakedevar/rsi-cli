//! #644: tell a job's owner and the appointed manager when a disk-floor hold
//! has persisted, so reclaim happens before the worker's own wake times out
//! (remainder of #582; the cargo-slot client is Python-only, so the daemon
//! watches the governor and records the notice here).
//!
//! One *hold* is one continuous stretch in which the host disk floor blocked a
//! job's governor wait; the governor's `disk_wait_started_at` names it.
//! [`Store::record_disk_hold_notice`] writes at most one notice per
//! `(job, hold)` to each recipient:
//!
//! - the job owner's agent mail, while the owner session is live;
//! - the project's manager inbox (`ledger_change`, subject
//!   `disk_floor_hold:<job>`, version = the hold start), unless the owner is
//!   that manager itself (its mail already tells it).
//!
//! Both routes are idempotent on the hold, so a repeated pass or a restarted
//! daemon records nothing new, and a later hold of the same job (a new start)
//! notifies again. Nothing here changes the floor or the job's admitted
//! timeout.

use super::Store;
use crate::error::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_coordination::AgentSendMessageRequestV1;
use rsi_common::types::SessionStatus;
use rusqlite::{Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

/// `record_kind` of the notice.
pub const DISK_HOLD_NOTICE: &str = "disk_floor_hold";
/// Subject prefix of the project-wide manager notice: `disk_floor_hold:<job>`.
pub const DISK_HOLD_SUBJECT_PREFIX: &str = "disk_floor_hold:";

/// One persisting disk-floor hold of one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskHoldReport {
    pub job_id: Uuid,
    pub owner_session_id: Uuid,
    /// When the floor first held this wait (identity of the hold).
    pub held_since: DateTime<Utc>,
    /// Whole seconds the hold had lasted when the notice was cut.
    pub held_secs: u64,
    pub free_bytes: u64,
    pub floor_bytes: u64,
    /// Terminal-worker build caches the reclaim sweep would take now.
    pub reclaimable_candidates: u32,
}

/// Where the notice went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskHoldRoute {
    /// New agent mail to the job owner.
    pub owner_mail: bool,
    /// New manager-inbox notice.
    pub manager_notice: bool,
}

impl DiskHoldRoute {
    /// Whether this pass wrote anything.
    #[must_use]
    pub fn any(self) -> bool {
        self.owner_mail || self.manager_notice
    }
}

impl DiskHoldReport {
    /// The identity of the hold the notice is deduplicated on.
    #[must_use]
    pub fn hold_key(&self) -> String {
        self.held_since.to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    fn subject(&self) -> String {
        format!("{DISK_HOLD_SUBJECT_PREFIX}{}", self.job_id)
    }

    /// The typed record the manager notice carries.
    #[must_use]
    pub fn state(&self) -> Value {
        json!({
            "record_kind": DISK_HOLD_NOTICE,
            "job_id": self.job_id,
            "owner_session_id": self.owner_session_id,
            "held_since": self.hold_key(),
            "held_secs": self.held_secs,
            "free_bytes": self.free_bytes,
            "floor_bytes": self.floor_bytes,
            "reclaimable_candidates": self.reclaimable_candidates,
            "detail": self.detail(),
        })
    }

    /// The notice text: the numbers and what to do, nothing periodic.
    #[must_use]
    pub fn detail(&self) -> String {
        let gib = |bytes: u64| bytes as f64 / 1_073_741_824.0;
        let next = if self.reclaimable_candidates == 0 {
            "the build-cache reclaim sweep has no eligible target, so free space by hand \
             (`scripts/clean-target` on a finished sandbox) or cancel the job"
        } else {
            "the build-cache reclaim sweep has eligible targets; free space or let it run"
        };
        format!(
            "[rsi daemon, #644 disk_floor_hold] Job {job} has been held by the host disk floor \
             for {mins} min (since {since}): {free:.1} GiB free ({free_bytes} bytes) against a \
             {floor:.1} GiB floor ({floor_bytes} bytes); {candidates} reclaimable build-cache \
             candidate(s). The floor and the job's admitted timeout are unchanged and the job \
             compiles nothing until space is freed; {next}. This is the only notice for this \
             hold. Live state: AgentGetJob {job} (`hold`).",
            job = self.job_id,
            mins = self.held_secs / 60,
            since = self.hold_key(),
            free = gib(self.free_bytes),
            free_bytes = self.free_bytes,
            floor = gib(self.floor_bytes),
            floor_bytes = self.floor_bytes,
            candidates = self.reclaimable_candidates,
        )
    }
}

impl Store {
    /// Route one persisting-hold notice (see the module docs). Idempotent per
    /// `(job, hold)`.
    ///
    /// # Errors
    /// A persistence error; the caller retries on its next pass.
    pub fn record_disk_hold_notice(&self, report: &DiskHoldReport) -> Result<DiskHoldRoute> {
        let owner = self.get_session(report.owner_session_id)?;
        let owner_live = owner.as_ref().is_some_and(|session| {
            matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
            )
        });
        let mut route = DiskHoldRoute::default();
        if owner_live {
            route.owner_mail = self.send_disk_hold_owner_mail(report)?;
        }
        let Some(project_id) = owner.and_then(|session| session.project_id) else {
            return Ok(route);
        };
        route.manager_notice = self.record_disk_hold_manager_notice(project_id, report)?;
        Ok(route)
    }

    fn send_disk_hold_owner_mail(&self, report: &DiskHoldReport) -> Result<bool> {
        let owner = report.owner_session_id;
        let idempotency_key = format!("disk-floor-hold:{}:{}", report.job_id, report.hold_key());
        if self.agent_message_key_accepted(owner, &idempotency_key)? {
            return Ok(false);
        }
        let outcome = self.accept_agent_message(
            owner,
            None,
            &AgentSendMessageRequestV1 {
                target_session_id: owner,
                message: report.detail(),
                idempotency_key,
                expires_at: None,
            },
        )?;
        Ok(matches!(
            outcome,
            super::agent_coordination::AcceptAgentMessageOutcome::Accepted(_)
        ))
    }

    fn record_disk_hold_manager_notice(
        &self,
        project_id: Uuid,
        report: &DiskHoldReport,
    ) -> Result<bool> {
        let Some(config) = self.get_harness_manager_notice_config(project_id)? else {
            return Ok(false);
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self
                .get_harness_manager_policy(project_id)?
                .is_none_or(|policy| policy.revoked)
            || config.manager_session_id == report.owner_session_id
        {
            return Ok(false);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (job_id, _, _) = self.manager_action_watch_identity(&config);
        let subject = report.subject();
        let version = report.hold_key();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject, version],
            |row| row.get(0),
        )?;
        if exists {
            tx.commit()?;
            return Ok(false);
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
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_coordinator::tests::fixture;
    use crate::test_support::make_test_session;
    use chrono::Duration;
    use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerPolicyV2};

    fn report(owner: Uuid, job: Uuid, held_since: DateTime<Utc>) -> DiskHoldReport {
        DiskHoldReport {
            job_id: job,
            owner_session_id: owner,
            held_since,
            held_secs: 200,
            free_bytes: 19_000_000_000,
            floor_bytes: 32_212_254_720,
            reclaimable_candidates: 2,
        }
    }

    fn count(store: &Store, sql: &str, arg: &str) -> i64 {
        store.conn.query_row(sql, [arg], |row| row.get(0)).unwrap()
    }

    fn mail_count(store: &Store, owner: Uuid) -> i64 {
        count(
            store,
            "SELECT COUNT(*) FROM agent_messages WHERE target_session_id=?1",
            &owner.to_string(),
        )
    }

    fn manager_count(store: &Store, job: Uuid) -> i64 {
        count(
            store,
            "SELECT COUNT(*) FROM harness_manager_notices
             WHERE kind='ledger_change' AND subject_id=?1",
            &format!("{DISK_HOLD_SUBJECT_PREFIX}{job}"),
        )
    }

    fn running_owner(store: &Store, project: Option<Uuid>) -> Uuid {
        let mut session = make_test_session();
        session.status = SessionStatus::Running;
        session.project_id = project;
        store.insert_session(&session).unwrap();
        session.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn one_hold_notifies_owner_and_manager_once_and_a_second_hold_again_644() {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(
            &store,
            ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        );
        let owner = running_owner(&store, Some(config.project_id));
        let job = Uuid::new_v4();
        let first = report(owner, job, Utc::now() - Duration::minutes(4));

        let route = store.record_disk_hold_notice(&first).unwrap();
        assert_eq!(
            route,
            DiskHoldRoute {
                owner_mail: true,
                manager_notice: true
            }
        );
        // A repeated pass and a changed reading of the same hold add nothing.
        let mut grown = first.clone();
        grown.held_secs = 900;
        grown.free_bytes = 1;
        assert!(!store.record_disk_hold_notice(&first).unwrap().any());
        assert!(!store.record_disk_hold_notice(&grown).unwrap().any());
        assert_eq!(mail_count(&store, owner), 1);
        assert_eq!(manager_count(&store, job), 1);

        let state: String = store
            .conn
            .query_row(
                "SELECT state_json FROM harness_manager_notices WHERE subject_id=?1",
                [format!("{DISK_HOLD_SUBJECT_PREFIX}{job}")],
                |row| row.get(0),
            )
            .unwrap();
        let state: Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["record_kind"], DISK_HOLD_NOTICE);
        assert_eq!(state["job_id"], job.to_string());
        assert_eq!(state["free_bytes"], 19_000_000_000_u64);
        assert_eq!(state["floor_bytes"], 32_212_254_720_u64);
        assert_eq!(state["reclaimable_candidates"], 2);

        // The same job held again later is a new hold and notifies again.
        let second = report(owner, job, Utc::now());
        assert_eq!(
            store.record_disk_hold_notice(&second).unwrap(),
            DiskHoldRoute {
                owner_mail: true,
                manager_notice: true
            }
        );
        assert_eq!(mail_count(&store, owner), 2);
        assert_eq!(manager_count(&store, job), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn the_mail_text_names_the_job_the_free_space_the_floor_and_the_candidates_644() {
        let detail = report(Uuid::new_v4(), Uuid::from_u128(9), Utc::now()).detail();
        for needle in [
            &Uuid::from_u128(9).to_string(),
            "17.7 GiB free",
            "30.0 GiB floor",
            "2 reclaimable",
            "3 min",
        ] {
            assert!(detail.contains(needle), "{needle} missing: {detail}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn an_owner_without_a_manager_is_mailed_and_nothing_else_is_recorded_644() {
        let store = Store::open_in_memory().unwrap();
        let owner = running_owner(&store, Some(Uuid::new_v4()));
        let job = Uuid::new_v4();
        let route = store
            .record_disk_hold_notice(&report(owner, job, Utc::now()))
            .unwrap();
        assert_eq!(
            route,
            DiskHoldRoute {
                owner_mail: true,
                manager_notice: false
            }
        );
        assert_eq!(manager_count(&store, job), 0);
    }
}
