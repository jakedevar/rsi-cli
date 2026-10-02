//! #953: a superseded session lineage never keeps a live resume wake.
//!
//! A `resume` wake chases the published rotation lineage to its tip (A8 §9
//! Q1), so a wake left on a replaced lead or on any earlier hop of its lineage
//! keeps waking whoever holds that lineage later, with a stale message, and a
//! chase that stops at its depth cap resumes a superseded hop on its old
//! provider. A lead handover disables them in the same transaction as its lead
//! CAS; the maintenance sweep retires the rest (rotation leftovers and wakes
//! whose target is gone). Rows are disabled, never deleted. Manager notice
//! watches and the wakes of an open capacity incident keep their own owners.

use super::super::Store;
use super::super::harness_manager_v2::now;
use super::{ManagerActionClaimV2, ManagerActionOriginV2, action_epic, parse_id};
use crate::error::Result;
use rsi_common::types::SessionStatus;
use rusqlite::{Transaction, TransactionBehavior, params};
use serde_json::json;
use uuid::Uuid;

/// Hops walked up `continued_from` from a replaced lead. Deeper than the
/// published-tip chase cap, so wakes the chase can no longer resolve are
/// retired too.
const ANCESTRY_CAP: usize = 256;

/// Manager event recording the resume wakes a lead handover retired. Keyed
/// by the action id, readable through the manager's event inspection.
const LEAD_WAKES_RETIRED_EVENT: &str = "lead_wakes_retired";

/// Enabled resume wakes whose own owner is not the ordinary scheduler.
const RETIRABLE_RESUME_WAKE: &str = "j.enabled=1 AND j.wake_mode='resume'
    AND NOT EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=j.id)
    AND NOT EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents i
                   WHERE i.state='open' AND (i.wake_job_id=j.id OR i.program_guard_job_id=j.id))";

impl Store {
    /// A succeeded `replace_lead`/`retry_lead` retires the outgoing lead's
    /// lineage wakes inside its lead-CAS transaction and, when any were
    /// retired, records their ids in a [`LEAD_WAKES_RETIRED_EVENT`] manager
    /// event for this action.
    pub(crate) fn retire_handover_lineage_resume_wakes_on(
        &self,
        claim: &ManagerActionClaimV2,
        predecessor: Uuid,
    ) -> Result<Vec<Uuid>> {
        let retired = self.retire_superseded_lineage_resume_wakes_on(predecessor)?;
        if retired.is_empty() {
            return Ok(retired);
        }
        let op = &claim.operation;
        let actor = match op.context.origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller.to_string()),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        let payload = json!({
            "operation_id": claim.id(),
            "epic_id": action_epic(claim.action()),
            "predecessor_session_id": predecessor,
            "retired_wake_job_ids": retired,
        });
        self.conn.execute(
            "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                op.project_id.to_string(),
                op.manager_session_id.to_string(),
                op.scope_version,
                actor,
                LEAD_WAKES_RETIRED_EVENT,
                claim.id().to_string(),
                op.receipt.row_version + 1,
                serde_json::to_string(&payload)?,
                now()
            ],
        )?;
        tracing::info!(
            operation_id = %claim.id(),
            predecessor = %predecessor,
            job_ids = ?retired,
            "lead handover retired superseded-lineage resume wakes"
        );
        Ok(retired)
    }

    /// Disable every retirable resume wake that targets `predecessor` or an
    /// earlier hop of the lineage that ends at it. An ancestor whose published
    /// successor is a different branch is not part of this lineage, so the
    /// walk stops there. Returns the disabled job ids, sorted. Runs on the
    /// caller's connection, inside its transaction.
    pub(crate) fn retire_superseded_lineage_resume_wakes_on(
        &self,
        predecessor: Uuid,
    ) -> Result<Vec<Uuid>> {
        let mut lineage = vec![predecessor];
        let mut hop = predecessor;
        for _ in 0..ANCESTRY_CAP {
            let Some(parent) = self.get_session(hop)?.and_then(|s| s.continued_from) else {
                break;
            };
            if lineage.contains(&parent)
                || self.find_published_rotation_successor(parent)? != Some(hop)
            {
                break;
            }
            lineage.push(parent);
            hop = parent;
        }
        let mut retired = Vec::new();
        for session in lineage {
            retired.extend(self.retirable_resume_wakes_for(session)?);
        }
        self.disable_retired_resume_wakes(&mut retired)?;
        Ok(retired)
    }

    /// Maintenance sweep: disable every retirable resume wake whose target no
    /// longer exists, is Archived or Deleted, or has an established published
    /// lineage successor (a Starting successor may still fail, which would
    /// make the target the tip again). Returns the disabled job ids, sorted.
    pub(crate) fn sweep_superseded_resume_wakes(&self) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let candidates: Vec<(String, Option<String>)> = {
            let mut statement = self.conn.prepare(&format!(
                "SELECT j.id, j.wake_session_id FROM scheduled_jobs j
                 WHERE {RETIRABLE_RESUME_WAKE} ORDER BY j.id"
            ))?;
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?
        };
        let mut retired = Vec::new();
        for (job, target) in candidates {
            let job = parse_id(&job)?;
            let superseded = match target.as_deref().map(parse_id).transpose()? {
                None => false,
                Some(target) => match self.get_session(target)? {
                    None => true,
                    Some(session)
                        if matches!(
                            session.status,
                            SessionStatus::Archived | SessionStatus::Deleted
                        ) =>
                    {
                        true
                    }
                    Some(_) => match self.find_published_rotation_successor(target)? {
                        Some(successor) => self
                            .get_session(successor)?
                            .is_some_and(|s| s.status != SessionStatus::Starting),
                        None => false,
                    },
                },
            };
            if superseded {
                retired.push(job);
            }
        }
        self.disable_retired_resume_wakes(&mut retired)?;
        tx.commit()?;
        Ok(retired)
    }

    fn retirable_resume_wakes_for(&self, session: Uuid) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT j.id FROM scheduled_jobs j
             WHERE j.wake_session_id=?1 AND {RETIRABLE_RESUME_WAKE} ORDER BY j.id"
        ))?;
        let ids: Vec<String> = statement
            .query_map([session.to_string()], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        ids.iter().map(|id| parse_id(id)).collect()
    }

    fn disable_retired_resume_wakes(&self, retired: &mut Vec<Uuid>) -> Result<()> {
        retired.sort();
        retired.dedup();
        let stamp = now();
        for id in retired.iter() {
            self.conn.execute(
                "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                params![id.to_string(), stamp],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    use crate::session::harness::tools::schedule_wake::{
        ScheduleWakeRequest, build_agent_scheduled_job,
    };

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    fn session(store: &Store, status: SessionStatus, continued_from: Option<Uuid>) -> Uuid {
        let mut row = crate::session::agent_verbs::tests::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/tmp/issue-953"),
        );
        row.status = status;
        row.continued_from = continued_from;
        store.insert_session(&row).unwrap();
        row.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    fn wake(store: &Store, owner: Uuid, mode: &str) -> Uuid {
        let job = build_agent_scheduled_job(ScheduleWakeRequest {
            message: "Continue".into(),
            in_seconds: Some(600),
            at: None,
            name: None,
            every_seconds: (mode == "resume").then_some(600),
            mode: Some(mode.into()),
            working_dir: std::path::PathBuf::from("/tmp/issue-953"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(owner),
            watch_session_id: (mode == "on_terminal").then(Uuid::new_v4),
        })
        .unwrap();
        store.insert_scheduled_job(&job).unwrap();
        job.id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    fn enabled(store: &Store, job: Uuid) -> bool {
        store
            .get_scheduled_job(&job)
            .unwrap()
            .expect("disabled, never deleted")
            .enabled
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
        ids.sort();
        ids
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn sweep_retires_resume_wakes_of_superseded_or_gone_targets_only() {
        let store = Store::open_in_memory().unwrap();
        let rotated = session(&store, SessionStatus::Completed, None);
        let tip = session(&store, SessionStatus::Running, Some(rotated));
        let archived = session(&store, SessionStatus::Archived, None);
        let rotating = session(&store, SessionStatus::Completed, None);
        session(&store, SessionStatus::Starting, Some(rotating));

        let on_rotated = wake(&store, rotated, "resume");
        let on_archived = wake(&store, archived, "resume");
        let on_tip = wake(&store, tip, "resume");
        let on_rotating = wake(&store, rotating, "resume");
        let watch_on_rotated = wake(&store, rotated, "on_terminal");

        assert_eq!(
            store.sweep_superseded_resume_wakes().unwrap(),
            sorted(vec![on_rotated, on_archived])
        );
        assert!(!enabled(&store, on_rotated));
        assert!(!enabled(&store, on_archived));
        assert!(enabled(&store, on_tip));
        // A Starting successor may still fail and hand the tip back.
        assert!(enabled(&store, on_rotating));
        assert!(enabled(&store, watch_on_rotated));
        // Idempotent: a second pass finds nothing new.
        assert!(store.sweep_superseded_resume_wakes().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn handover_retires_the_lineage_that_ends_at_the_predecessor_only() {
        let store = Store::open_in_memory().unwrap();
        let root = session(&store, SessionStatus::Completed, None);
        let middle = session(&store, SessionStatus::Completed, Some(root));
        let predecessor = session(&store, SessionStatus::Completed, Some(middle));
        let on_root = wake(&store, root, "resume");
        let on_middle = wake(&store, middle, "resume");
        let on_predecessor = wake(&store, predecessor, "resume");
        // `root` also published a different branch: its wakes chase that
        // branch, so they are not part of the predecessor's lineage.
        let branch = session(&store, SessionStatus::Running, Some(root));
        store
            .insert_rotation_event(
                root,
                "rotation-953",
                "completed",
                "completed",
                Some(&json!({ "successor_id": branch }).to_string()),
            )
            .unwrap();

        assert_eq!(
            store
                .retire_superseded_lineage_resume_wakes_on(predecessor)
                .unwrap(),
            sorted(vec![on_middle, on_predecessor])
        );
        assert!(enabled(&store, on_root));
        assert!(!enabled(&store, on_middle));
        assert!(!enabled(&store, on_predecessor));
    }
}
