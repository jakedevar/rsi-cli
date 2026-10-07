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
use super::super::harness_manager_v2::{now, refused};
use super::{ManagerActionClaimV2, ManagerActionOriginV2, action_epic, parse_id};
use crate::error::Result;
use rsi_common::harness_manager::HarnessManagerConfigV1;
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
    pub fn sweep_superseded_resume_wakes(&self) -> Result<Vec<Uuid>> {
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

    /// #1553: an appointment of another session displaces the seat's holder
    /// and starts a new anchor, so nothing the displaced holder armed can reach
    /// the new seat through the rotation lineage (a rotation successor shares
    /// the anchor and inherits through it). The scope save calls this in its
    /// own transaction, once the new anchor is published, to hand the displaced
    /// lineage's pending deliverables to the project's seat:
    ///
    /// - enabled terminal watches (`on_terminal:`) on the displaced lineage move
    ///   to `new_seat`; a watch the new seat already holds on the same session
    ///   is disabled instead, so one terminal fires one wake;
    /// - a live deploy (`staged`/`restarting`) changes owner, so its outcome
    ///   wake is written for the new seat;
    /// - a deploy outcome wake already written but not yet fired moves to
    ///   `new_seat`;
    /// - non-terminal daemon jobs and their pending batch waits move to
    ///   `new_seat` (#1587);
    /// - unsettled rolling-queue outcomes and their unfired wakes move to
    ///   `new_seat` while enqueue/replay identity stays fixed (#1604).
    ///
    /// Manager notice watches and unrelated wakes keep their own owners. Rows
    /// are retargeted or disabled, never deleted. Returns the retargeted
    /// `(terminal watches, deploys, deploy wakes)`.
    pub(crate) fn transfer_displaced_seat_wakes_on(
        &self,
        displaced: &HarnessManagerConfigV1,
        new_seat: Uuid,
    ) -> Result<(usize, usize, usize)> {
        let origin = displaced.manager_session_id;
        let mut lineage = super::super::harness_manager::manager_lineage_hops_on(
            &self.conn,
            origin,
            displaced.project_id,
        );
        lineage.retain(|session| *session != new_seat);
        let stamp = now();
        let (mut watches, mut deploys, mut wakes) = (0, 0, 0);
        for old in lineage {
            super::super::agent_jobs::transfer_manager_jobs_on(&self.conn, old, new_seat)?;
            let old = old.to_string();
            let target = new_seat.to_string();
            // Deploy replay/cancel identity is (owner, key). A historical
            // deployment of the new owner can occupy that key. Refuse the
            // entire appointment rather than publish a seat with an orphaned
            // live deploy or make cancel address a different deployment.
            let conflicting_deploy: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_deploys live
                 JOIN agent_deploys prior ON prior.owner_session_id=?2
                   AND prior.idempotency_digest=live.idempotency_digest AND prior.id<>live.id
                 WHERE live.owner_session_id=?1 AND live.state IN ('staged','restarting'))",
                params![old, target],
                |row| row.get(0),
            )?;
            if conflicting_deploy {
                return Err(refused("manager_deploy_transfer_key_conflict"));
            }
            let owned: Vec<(String, String)> = {
                let mut statement = self.conn.prepare(
                    "SELECT j.id, j.wake_mode FROM scheduled_jobs j
                     WHERE j.enabled=1 AND j.wake_session_id=?1 AND j.wake_mode LIKE 'on_terminal:%'
                       AND NOT EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=j.id)
                     ORDER BY j.id",
                )?;
                statement
                    .query_map([&old], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?
            };
            for (job, mode) in owned {
                let held: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
                     WHERE enabled=1 AND wake_session_id=?1 AND wake_mode=?2)",
                    params![target, mode],
                    |r| r.get(0),
                )?;
                let changed = if held {
                    self.conn.execute(
                        "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1",
                        params![job, stamp],
                    )?
                } else {
                    self.conn.execute(
                        "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
                         WHERE id=?1 AND enabled=1 AND wake_session_id=?4",
                        params![job, target, stamp, old],
                    )?
                };
                watches += usize::from(changed == 1 && !held);
            }
            deploys += self.conn.execute(
                "UPDATE agent_deploys SET owner_session_id=?2
                 WHERE owner_session_id=?1 AND state IN ('staged','restarting')",
                params![old, target],
            )?;
            wakes += self.conn.execute(
                "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3
                 WHERE enabled=1 AND wake_mode='resume' AND wake_session_id=?1
                   AND id IN (SELECT wake_job_id FROM agent_deploys WHERE wake_job_id IS NOT NULL)",
                params![old, target, stamp],
            )?;
        }
        Ok((watches, deploys, wakes))
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
    use crate::store_support::schedule_wake_job::{ScheduleWakeRequest, build_agent_scheduled_job};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    fn session(store: &Store, status: SessionStatus, continued_from: Option<Uuid>) -> Uuid {
        let mut row = crate::test_support::test_session(
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

    /// #1553: appointing another session displaces the seat's holder. What the
    /// displaced holder left armed follows the seat: its terminal watches (one
    /// per watched session, the new seat's own kept), its live deploy and the
    /// outcome wake of a settled deploy. Its own continuation wakes stay.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn appointing_another_session_hands_the_displaced_seats_wakes_to_the_new_seat() {
        use crate::store::agent_deploys::{NewDeploy, deploy_fingerprint_with};
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::agent_deploy::DeployState;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::ManagerPolicyV2;
        use rsi_common::types::SessionKind;

        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let old = config.manager_session_id;
        let mut successor = crate::test_support::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/tmp/1553"),
        );
        successor.session_kind = SessionKind::Standard;
        successor.project_id = Some(config.project_id);
        successor.status = SessionStatus::Completed;
        store.insert_session(&successor).unwrap();

        let watch_on = |owner: Uuid, watched: Uuid| {
            let job = build_agent_scheduled_job(ScheduleWakeRequest {
                message: "Child ended".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("on_terminal".into()),
                working_dir: std::path::PathBuf::from("/tmp/1553"),
                provider: None,
                model: None,
                project_id: None,
                origin_session_id: Some(owner),
                watch_session_id: Some(watched),
            })
            .unwrap();
            store.insert_scheduled_job(&job).unwrap();
            job.id
        };
        let moved = watch_on(old, Uuid::new_v4());
        let shared = Uuid::new_v4();
        let displaced_duplicate = watch_on(old, shared);
        let kept_duplicate = watch_on(successor.id, shared);
        let own_continuation = wake(&store, old, "resume");

        fn new_deploy(owner: Uuid, key: &str) -> NewDeploy<'_> {
            NewDeploy {
                id: Uuid::new_v4(),
                owner_session_id: owner,
                idempotency_key: key,
                sha: SHA,
                fingerprint: deploy_fingerprint_with(SHA, "x", 60, false),
                manifest: &[],
                max_wait_secs: 60,
                interrupt_workers: false,
            }
        }
        let settled = store
            .insert_agent_deploy(&new_deploy(old, "settled"), chrono::Utc::now())
            .unwrap();
        let outcome_wake = store
            .settle_agent_deploy(
                settled.id,
                DeployState::Failed,
                Some("test"),
                chrono::Utc::now(),
            )
            .unwrap()
            .expect("the settled deploy's outcome wake");
        let live = store
            .insert_agent_deploy(&new_deploy(old, "live"), chrono::Utc::now())
            .unwrap();

        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: config.project_id,
                session_id: successor.id,
                epic_ids: Some(vec![lead.parent_id.unwrap()]),
                group_ids: Vec::new(),
                expected_row_version: config.row_version,
            })
            .unwrap();

        let owner_of = |job: Uuid| {
            store
                .get_scheduled_job(&job)
                .unwrap()
                .expect("retargeted, never deleted")
                .wake_session_id
        };
        assert_eq!(owner_of(moved), Some(successor.id));
        assert!(enabled(&store, moved));
        assert!(!enabled(&store, displaced_duplicate));
        assert!(enabled(&store, kept_duplicate));
        assert_eq!(owner_of(kept_duplicate), Some(successor.id));
        assert_eq!(owner_of(outcome_wake), Some(successor.id));
        assert!(enabled(&store, outcome_wake));
        assert_eq!(owner_of(own_continuation), Some(old));
        assert!(enabled(&store, own_continuation));
        assert_eq!(
            store
                .get_agent_deploy(live.id)
                .unwrap()
                .unwrap()
                .owner_session_id,
            Some(successor.id)
        );
        assert_eq!(
            store
                .get_agent_deploy(settled.id)
                .unwrap()
                .unwrap()
                .owner_session_id,
            Some(old)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn seat_transfer_stops_at_a_rotation_that_changed_project_or_parent() {
        use crate::store::agent_deploys::{NewDeploy, deploy_fingerprint_with};
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::ManagerPolicyV2;

        for change_project in [true, false] {
            let store = Store::open_in_memory().unwrap();
            let (config, lead) = fixture(&store, ManagerPolicyV2::default());
            let old = config.manager_session_id;
            let mut rotated = store.get_session(old).unwrap().unwrap();
            rotated.id = Uuid::new_v4();
            rotated.continued_from = Some(old);
            rotated.rotation_depth += 1;
            store.insert_session(&rotated).unwrap();
            store
                .update_session_status(old, SessionStatus::Archived)
                .unwrap();
            store
                .record_harness_manager_rotation(old, rotated.id)
                .unwrap();

            let (other, _) = fixture(&store, ManagerPolicyV2::default());
            // A committed edge does not freeze its sessions' project/parent.
            // The old appointment's strict lineage now refuses this hop.
            if change_project {
                store
                    .conn
                    .execute(
                        "UPDATE sessions SET project_id=?2 WHERE id=?1",
                        params![rotated.id.to_string(), other.project_id.to_string()],
                    )
                    .unwrap();
            } else {
                store
                    .conn
                    .execute(
                        "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                        params![rotated.id.to_string(), lead.parent_id.unwrap().to_string()],
                    )
                    .unwrap();
            }
            assert!(store.manager_lineage_tip(old).is_err());
            let own_watch = wake(&store, old, "on_terminal");
            let foreign_watch = wake(&store, rotated.id, "on_terminal");
            let foreign_resume = wake(&store, rotated.id, "resume");
            const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
            let deploy = store
                .insert_agent_deploy(
                    &NewDeploy {
                        id: Uuid::new_v4(),
                        owner_session_id: rotated.id,
                        idempotency_key: "foreign-deploy",
                        sha: SHA,
                        fingerprint: deploy_fingerprint_with(SHA, "x", 60, false),
                        manifest: &[],
                        max_wait_secs: 60,
                        interrupt_workers: false,
                    },
                    chrono::Utc::now(),
                )
                .unwrap();
            let mut successor = store.get_session(old).unwrap().unwrap();
            successor.id = Uuid::new_v4();
            successor.status = SessionStatus::Completed;
            store.insert_session(&successor).unwrap();
            store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: config.project_id,
                    session_id: successor.id,
                    epic_ids: Some(vec![lead.parent_id.unwrap()]),
                    group_ids: Vec::new(),
                    expected_row_version: config.row_version,
                })
                .unwrap();
            assert_eq!(
                store
                    .get_scheduled_job(&own_watch)
                    .unwrap()
                    .unwrap()
                    .wake_session_id,
                Some(successor.id)
            );
            for job in [foreign_watch, foreign_resume] {
                let watch = store.get_scheduled_job(&job).unwrap().unwrap();
                assert_eq!(watch.wake_session_id, Some(rotated.id));
                assert!(watch.enabled);
            }
            assert_eq!(
                store
                    .get_agent_deploy(deploy.id)
                    .unwrap()
                    .unwrap()
                    .owner_session_id,
                Some(rotated.id)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn deploy_key_collision_rolls_back_the_seat_and_all_wake_transfers() {
        use crate::store::agent_deploys::{NewDeploy, deploy_fingerprint_with};
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::agent_deploy::DeployState;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::ManagerPolicyV2;

        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let old = config.manager_session_id;
        let mut successor = store.get_session(old).unwrap().unwrap();
        successor.id = Uuid::new_v4();
        store.insert_session(&successor).unwrap();
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let stage = |owner| {
            store
                .insert_agent_deploy(
                    &NewDeploy {
                        id: Uuid::new_v4(),
                        owner_session_id: owner,
                        idempotency_key: "reused-key",
                        sha: SHA,
                        fingerprint: deploy_fingerprint_with(SHA, "x", 60, true),
                        manifest: &[],
                        max_wait_secs: 60,
                        interrupt_workers: true,
                    },
                    chrono::Utc::now(),
                )
                .unwrap()
        };
        let prior = stage(successor.id);
        store
            .settle_agent_deploy(
                prior.id,
                DeployState::Failed,
                Some("test"),
                chrono::Utc::now(),
            )
            .unwrap();
        let live = stage(old);
        let watch = wake(&store, old, "on_terminal");
        let error = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: config.project_id,
                session_id: successor.id,
                epic_ids: Some(vec![lead.parent_id.unwrap()]),
                group_ids: Vec::new(),
                expected_row_version: config.row_version,
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("manager_deploy_transfer_key_conflict"),
            "{error}"
        );
        let held = store
            .get_harness_manager(config.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(held.manager_session_id, old);
        assert_eq!(held.row_version, config.row_version);
        let watch = store.get_scheduled_job(&watch).unwrap().unwrap();
        assert_eq!(watch.wake_session_id, Some(old));
        assert!(watch.enabled);
        assert_eq!(
            store
                .get_agent_deploy(live.id)
                .unwrap()
                .unwrap()
                .owner_session_id,
            Some(old)
        );
        assert!(store.agent_deploy_interrupt(live.id).unwrap().requested);
        // The retained seat can still cancel its own live deployment.
        assert_eq!(
            store
                .cancel_agent_deploy(old, "reused-key", SHA, chrono::Utc::now())
                .unwrap()
                .id,
            live.id
        );
    }
}
