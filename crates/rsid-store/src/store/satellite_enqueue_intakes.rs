//! Hub #3345: durable state of the satellite ENQUEUE intake.
//!
//! The hub daemon takes a parseable `ENQUEUE` satellite report in itself: it
//! verifies the commit, probes it against rolling and the queue, enqueues it
//! when clean and tells the satellite (`rsid::satellite::enqueue_intake`). This
//! module is the store half: one `satellite_enqueue_intakes` row per report
//! (inserted in the same transaction as the report's manager notice, so a
//! report is taken in exactly once), the state transitions, the reply
//! bookkeeping and the manager digest notice.
//!
//! The digest is one `ledger_change` notice per intake state
//! (`satellite_intake:<peer>:<report>`, version = the state). It is *routine*
//! (visible in the inbox, never a wake of its own) unless the manager must act
//! (`needs_attention`): a pre-merge review, a refusal, a stalled intake or a
//! source that wants a post-land review. A recorded report that the intake
//! owns is routine too (`auto_intake`); the digest and the satellite reply
//! carry everything the manager needs.

use super::Store;
use super::harness_manager_v2::now;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::satellite_intake::{
    INTAKE_NOTICE_RECORD_KIND, INTAKE_NOTICE_SUBJECT_PREFIX, IntakeStateV1, SatelliteEnqueueClaimV1,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

/// One intake row.
#[derive(Debug, Clone, PartialEq)]
pub struct EnqueueIntakeRow {
    pub peer_id: Uuid,
    pub report_id: Uuid,
    pub project_id: Uuid,
    pub source_commit: String,
    pub branch: Option<String>,
    pub issue: Option<i64>,
    pub fixes_issue: Option<i64>,
    pub fixes_tests: Vec<String>,
    pub state: IntakeStateV1,
    pub queue_entry_id: Option<Uuid>,
    pub outcome_code: Option<String>,
    pub outcome: Option<Value>,
    pub attempts: u32,
    pub notified_state: Option<IntakeStateV1>,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "peer_id,report_id,project_id,source_commit,branch,issue,fixes_issue,
    fixes_tests_json,state,queue_entry_id,outcome_code,outcome_json,attempts,notified_state,
    created_at";

fn uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| DaemonError::Store("invalid satellite intake id".into()))
}

fn read_row(row: &rusqlite::Row<'_>) -> Result<EnqueueIntakeRow> {
    let text = |index: usize| -> Result<String> { Ok(row.get::<_, String>(index)?) };
    let optional =
        |index: usize| -> Result<Option<String>> { Ok(row.get::<_, Option<String>>(index)?) };
    let state = IntakeStateV1::parse(&text(8)?)
        .ok_or_else(|| DaemonError::Store("invalid satellite intake state".into()))?;
    let notified_state = optional(13)?
        .map(|value| {
            IntakeStateV1::parse(&value)
                .ok_or_else(|| DaemonError::Store("invalid satellite intake notified state".into()))
        })
        .transpose()?;
    Ok(EnqueueIntakeRow {
        peer_id: uuid(&text(0)?)?,
        report_id: uuid(&text(1)?)?,
        project_id: uuid(&text(2)?)?,
        source_commit: text(3)?,
        branch: optional(4)?,
        issue: row.get(5)?,
        fixes_issue: row.get(6)?,
        fixes_tests: serde_json::from_str(&text(7)?)?,
        state,
        queue_entry_id: optional(9)?.map(|value| uuid(&value)).transpose()?,
        outcome_code: optional(10)?,
        outcome: optional(11)?
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        attempts: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(u32::MAX),
        notified_state,
        created_at: DateTime::parse_from_rfc3339(&text(14)?)
            .map_err(|_| DaemonError::Store("invalid satellite intake time".into()))?
            .with_timezone(&Utc),
    })
}

impl Store {
    /// Take one report in. Runs inside the caller's transaction (the report's
    /// notice insert). `false`: the report already has a row.
    pub(crate) fn insert_enqueue_intake(
        &self,
        peer_id: Uuid,
        report_id: Uuid,
        project_id: Uuid,
        claim: &SatelliteEnqueueClaimV1,
    ) -> Result<bool> {
        let stamped = now();
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO satellite_enqueue_intakes
             (peer_id,report_id,project_id,source_commit,branch,issue,fixes_issue,
              fixes_tests_json,state,created_at,updated_at,outcome_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'received',?9,?9,?10)",
            params![
                peer_id.to_string(),
                report_id.to_string(),
                project_id.to_string(),
                claim.source_commit,
                claim.branch,
                claim.issue,
                claim.fixes_issue,
                serde_json::to_string(&claim.fixes_tests)?,
                stamped,
                // Received-state options survive a daemon restart without
                // changing the schema. The decision replaces this outcome.
                claim
                    .expedite
                    .as_ref()
                    .map(|expedite| { serde_json::to_string(&json!({ "expedite": expedite })) })
                    .transpose()?,
            ],
        )?;
        Ok(inserted > 0)
    }

    /// The intakes the daemon still has to decide (`received`) or watch
    /// (`enqueued`), oldest first.
    pub fn open_enqueue_intakes(&self, limit: usize) -> Result<Vec<EnqueueIntakeRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM satellite_enqueue_intakes
             WHERE state IN ('received','enqueued')
             ORDER BY created_at,peer_id,report_id LIMIT ?1"
        ))?;
        let mut rows = statement.query(params![i64::try_from(limit).unwrap_or(i64::MAX)])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(read_row(row)?);
        }
        Ok(out)
    }

    /// The intakes whose satellite has not yet been told their state. An
    /// `enqueued` intake needs no reply (the satellite hears at the settle).
    pub fn enqueue_intakes_owing_reply(&self, limit: usize) -> Result<Vec<EnqueueIntakeRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM satellite_enqueue_intakes
             WHERE state IN ('conflicts','refused','needs_review','settled')
               AND notified_state IS NOT state
             ORDER BY created_at,peer_id,report_id LIMIT ?1"
        ))?;
        let mut rows = statement.query(params![i64::try_from(limit).unwrap_or(i64::MAX)])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(read_row(row)?);
        }
        Ok(out)
    }

    /// One intake by report.
    pub fn get_enqueue_intake(
        &self,
        peer_id: Uuid,
        report_id: Uuid,
    ) -> Result<Option<EnqueueIntakeRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM satellite_enqueue_intakes WHERE peer_id=?1 AND report_id=?2"
        ))?;
        let mut rows = statement.query(params![peer_id.to_string(), report_id.to_string()])?;
        rows.next()?.map(read_row).transpose()
    }

    /// Move an intake from `from` to `to`, recording the queue entry and the
    /// outcome. A compare-and-set: `false` when the row is no longer in `from`.
    pub fn transition_enqueue_intake(
        &self,
        row: &EnqueueIntakeRow,
        from: IntakeStateV1,
        to: IntakeStateV1,
        queue_entry_id: Option<Uuid>,
        outcome_code: Option<&str>,
        outcome: Option<&Value>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE satellite_enqueue_intakes
             SET state=?4,queue_entry_id=COALESCE(?5,queue_entry_id),outcome_code=?6,
                 outcome_json=?7,updated_at=?8
             WHERE peer_id=?1 AND report_id=?2 AND state=?3",
            params![
                row.peer_id.to_string(),
                row.report_id.to_string(),
                from.as_str(),
                to.as_str(),
                queue_entry_id.map(|id| id.to_string()),
                outcome_code,
                outcome.map(Value::to_string),
                now(),
            ],
        )?;
        Ok(changed > 0)
    }

    /// Count one failed attempt; returns the new count.
    pub fn bump_enqueue_intake_attempts(&self, row: &EnqueueIntakeRow) -> Result<u32> {
        self.conn.execute(
            "UPDATE satellite_enqueue_intakes SET attempts=attempts+1,updated_at=?3
             WHERE peer_id=?1 AND report_id=?2",
            params![row.peer_id.to_string(), row.report_id.to_string(), now()],
        )?;
        let attempts: i64 = self.conn.query_row(
            "SELECT attempts FROM satellite_enqueue_intakes WHERE peer_id=?1 AND report_id=?2",
            params![row.peer_id.to_string(), row.report_id.to_string()],
            |db| db.get(0),
        )?;
        Ok(u32::try_from(attempts).unwrap_or(u32::MAX))
    }

    /// The satellite was told `state`.
    pub fn mark_enqueue_intake_notified(
        &self,
        row: &EnqueueIntakeRow,
        state: IntakeStateV1,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE satellite_enqueue_intakes SET notified_state=?3,updated_at=?4
             WHERE peer_id=?1 AND report_id=?2",
            params![
                row.peer_id.to_string(),
                row.report_id.to_string(),
                state.as_str(),
                now()
            ],
        )?;
        Ok(())
    }

    /// The branch the satellite named for the intake that queued `entry`.
    pub fn enqueue_intake_branch_for_entry(&self, entry: Uuid) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT branch FROM satellite_enqueue_intakes WHERE queue_entry_id=?1",
                [entry.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// #600 (hub #3604): whether a satellite reported this queue entry's source
    /// (an intake row names the entry, or the same source commit). The
    /// commits of such a source carry the satellite project's Issue numbers,
    /// which are not this project's display numbers.
    pub fn entry_is_satellite_sourced(&self, entry: Uuid, source_commit: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM satellite_enqueue_intakes
             WHERE queue_entry_id=?1 OR source_commit=?2)",
            params![entry.to_string(), source_commit],
            |row| row.get(0),
        )?)
    }

    /// Hub #3794: whether a satellite reported exactly this commit as an
    /// ENQUEUE source. A hub entry that merges a satellite's work publishes that
    /// commit among its own, and its subject carries the satellite's numbers.
    pub fn commit_is_satellite_reported(&self, sha: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM satellite_enqueue_intakes WHERE source_commit=?1)",
            [sha],
            |row| row.get(0),
        )?)
    }

    /// Where the hub addresses a reply to `peer`: its only declared project
    /// (the satellite resolves that project's current manager), else its only
    /// declared session. `None` when the scope is empty or ambiguous: the hub
    /// never guesses a recipient.
    pub fn satellite_reply_address(&self, peer_id: Uuid) -> Result<Option<Uuid>> {
        let projects = self.satellite_peer_project_scope(peer_id)?;
        if let [only] = projects.as_slice() {
            return Ok(Some(only.0));
        }
        if !projects.is_empty() {
            return Ok(None);
        }
        let mut statement = self.conn.prepare(
            "SELECT remote_session_id FROM satellite_peer_scope WHERE peer_id=?1 LIMIT 2",
        )?;
        let sessions = statement
            .query_map([peer_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        match sessions.as_slice() {
            [only] => Ok(Some(uuid(only)?)),
            _ => Ok(None),
        }
    }

    /// Record the digest notice of an intake state for the project's current
    /// manager. One notice per `(intake, state)`; `needs_attention` makes it a
    /// wake, otherwise it is routine. `Ok(false)`: no live manager, or already
    /// recorded.
    pub fn record_enqueue_intake_notice(
        &self,
        row: &EnqueueIntakeRow,
        needs_attention: bool,
        detail: &str,
    ) -> Result<bool> {
        let Some(config) = self.get_harness_manager_notice_config(row.project_id)? else {
            return Ok(false);
        };
        if config.current_session_id.is_none()
            || config.is_revoked()
            || self
                .get_harness_manager_policy(row.project_id)?
                .is_none_or(|policy| policy.revoked)
        {
            return Ok(false);
        }
        let (job_id, _, _) = self.manager_action_watch_identity(&config);
        let subject = format!(
            "{INTAKE_NOTICE_SUBJECT_PREFIX}{}:{}",
            row.peer_id, row.report_id
        );
        let version = row.state.as_str();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject, version],
            |db| db.get(0),
        )?;
        if exists {
            return Ok(false);
        }
        self.ensure_manager_action_watch(&config, version)?;
        let recorded_at = now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,'ledger_change',?6,?7,?8,?9,?9)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                subject,
                version,
                serde_json::to_string(&json!({
                    "record_kind": INTAKE_NOTICE_RECORD_KIND,
                    "untrusted": true,
                    "informational_only": true,
                    "peer_id": row.peer_id,
                    "report_id": row.report_id,
                    "source_commit": row.source_commit,
                    "branch": row.branch,
                    "issue": row.issue,
                    "state": version,
                    "queue_entry_id": row.queue_entry_id,
                    "outcome_code": row.outcome_code,
                    "outcome": row.outcome,
                    "needs_attention": needs_attention,
                    "detail": detail,
                }))?,
                recorded_at,
            ],
        )?;
        self.refresh_manager_notice_job(job_id)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::satellite_intake::parse_enqueue_claim;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn claim() -> SatelliteEnqueueClaimV1 {
        parse_enqueue_claim(&format!(
            "ENQUEUE {SHA} issue=#9 branch=rsi/x fixes_tests=a::b expedite=qa-repair"
        ))
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn an_intake_is_taken_in_once_and_walks_its_states() {
        let store = Store::open_in_memory().unwrap();
        let (peer, report, project) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert!(
            store
                .insert_enqueue_intake(peer, report, project, &claim())
                .unwrap()
        );
        assert!(
            !store
                .insert_enqueue_intake(peer, report, project, &claim())
                .unwrap(),
            "a replay of the report adds no second intake"
        );
        let open = store.open_enqueue_intakes(10).unwrap();
        assert_eq!(open.len(), 1);
        let row = &open[0];
        assert_eq!(row.state, IntakeStateV1::Received);
        assert_eq!(row.source_commit, SHA);
        assert_eq!(row.issue, Some(9));
        assert_eq!(row.fixes_tests, vec!["a::b"]);
        assert_eq!(
            row.outcome.as_ref().unwrap()["expedite"]["reason"],
            "qa-repair"
        );
        assert!(store.enqueue_intakes_owing_reply(10).unwrap().is_empty());

        let entry = Uuid::new_v4();
        assert!(
            store
                .transition_enqueue_intake(
                    row,
                    IntakeStateV1::Received,
                    IntakeStateV1::Enqueued,
                    Some(entry),
                    None,
                    None
                )
                .unwrap()
        );
        assert!(
            !store
                .transition_enqueue_intake(
                    row,
                    IntakeStateV1::Received,
                    IntakeStateV1::Refused,
                    None,
                    Some("x"),
                    None
                )
                .unwrap(),
            "a transition is a compare-and-set"
        );
        assert!(store.enqueue_intakes_owing_reply(10).unwrap().is_empty());
        assert_eq!(
            store.enqueue_intake_branch_for_entry(entry).unwrap(),
            Some("rsi/x".to_owned())
        );

        let enqueued = store.get_enqueue_intake(peer, report).unwrap().unwrap();
        assert!(
            store
                .transition_enqueue_intake(
                    &enqueued,
                    IntakeStateV1::Enqueued,
                    IntakeStateV1::Settled,
                    None,
                    Some("published"),
                    Some(&json!({"landed_sha": SHA}))
                )
                .unwrap()
        );
        let settled = store.get_enqueue_intake(peer, report).unwrap().unwrap();
        assert_eq!(settled.queue_entry_id, Some(entry));
        assert_eq!(settled.outcome.as_ref().unwrap()["landed_sha"], SHA);
        let owed = store.enqueue_intakes_owing_reply(10).unwrap();
        assert_eq!(owed.len(), 1);
        store
            .mark_enqueue_intake_notified(&owed[0], IntakeStateV1::Settled)
            .unwrap();
        assert!(store.enqueue_intakes_owing_reply(10).unwrap().is_empty());
        assert!(store.open_enqueue_intakes(10).unwrap().is_empty());
        assert_eq!(store.bump_enqueue_intake_attempts(&settled).unwrap(), 1);
        assert_eq!(store.bump_enqueue_intake_attempts(&settled).unwrap(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn the_reply_address_is_the_only_declared_scope_and_never_a_guess() {
        use rsi_common::satellite::{
            SatellitePeerConfigV1, SatellitePutPeerRequestV1, SatelliteUuidV1,
        };
        let store = Store::open_in_memory().unwrap();
        let peer = Uuid::new_v4();
        assert_eq!(store.satellite_reply_address(peer).unwrap(), None);
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "work laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(peer)),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                std::path::Path::new("/tmp/satellites"),
            )
            .unwrap();
        let (a, b, session) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        store
            .put_satellite_peer_scope_entries(2, peer, &[], &[SatelliteUuidV1(a)])
            .unwrap();
        assert_eq!(store.satellite_reply_address(peer).unwrap(), Some(a));
        store
            .put_satellite_peer_scope_entries(
                3,
                peer,
                &[],
                &[SatelliteUuidV1(a), SatelliteUuidV1(b)],
            )
            .unwrap();
        assert_eq!(
            store.satellite_reply_address(peer).unwrap(),
            None,
            "two declared projects are ambiguous"
        );
        store
            .put_satellite_peer_scope_entries(4, peer, &[SatelliteUuidV1(session)], &[])
            .unwrap();
        assert_eq!(store.satellite_reply_address(peer).unwrap(), Some(session));
    }
}
