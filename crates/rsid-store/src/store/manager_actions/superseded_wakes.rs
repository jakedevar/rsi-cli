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
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::ManagerCancelledWakesV1;
use rsi_common::types::SessionStatus;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use uuid::Uuid;

/// Hops walked up `continued_from` from a replaced lead. Deeper than the
/// published-tip chase cap, so wakes the chase can no longer resolve are
/// retired too.
const ANCESTRY_CAP: usize = 256;

/// Manager event recording the resume wakes a lead handover retired. Keyed
/// by the action id, readable through the manager's event inspection.
const LEAD_WAKES_RETIRED_EVENT: &str = "lead_wakes_retired";

/// Manager event recording how a `succeed_manager` publication settled the
/// displaced seat's resume wakes: the ids handed to the successor, the ids
/// retired with the displaced seat, and the reason. Keyed by the operation id.
pub(crate) const SUCCESSION_WAKES_EVENT: &str = "succession_wakes_settled";

/// Reason recorded for a resume wake a succession retires.
pub(crate) const SUCCESSION_WAKES_REASON: &str = "manager_succession_displaced_seat";

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

    /// Enabled wakes of `predecessor` that a seat change could hand to its
    /// successor. Read before the publication, inside its transaction.
    pub(crate) fn succession_carried_wake_ids_on(&self, predecessor: Uuid) -> Result<Vec<Uuid>> {
        super::super::manager_obligations::carried_wake_ids_on(&self.conn, predecessor)
    }

    /// A succeeded `succeed_manager` settles the displaced seat's resume wakes
    /// inside its publication transaction (#2652). The publication hands the
    /// seat's durable obligations (job and deploy outcome wakes, predicate waits,
    /// watches) to the successor without its generic backstops (the transfer
    /// skips them while this succession's root row is `established`, unlike an
    /// automatic rotation, #2511); every
    /// resume wake still targeting the displaced lineage is then cancelled: its
    /// generic or safety-net wakes, a `keepalive-*`/`background-process-*` row
    /// created after admission, an earlier rotation hop. Rows are disabled, never
    /// deleted; protected rows (manager watches, wakes of an open capacity
    /// incident) keep their owners. `candidates` are the pre-publication wakes;
    /// only those the successor now owns are recorded as transferred. Returns the cancelled job ids, sorted, and
    /// records both lists with the reason in a [`SUCCESSION_WAKES_EVENT`]
    /// manager event keyed by the operation id, from which the final receipt's
    /// `cancelled_wakes` is read.
    pub(crate) fn settle_succession_displaced_wakes_on(
        &self,
        claim: &ManagerActionClaimV2,
        predecessor: Uuid,
        successor: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>> {
        let mut handed = Vec::new();
        for id in candidates {
            let owner: Option<Option<String>> = self
                .conn
                .query_row(
                    "SELECT wake_session_id FROM scheduled_jobs WHERE id=?1 AND enabled=1 AND wake_mode='resume'",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            if matches!(owner, Some(Some(ref owner)) if *owner == successor.to_string()) {
                handed.push(*id);
            }
        }
        handed.sort();
        let retired = self.retire_superseded_lineage_resume_wakes_on(predecessor)?;
        if handed.is_empty() && retired.is_empty() {
            return Ok(retired);
        }
        let op = &claim.operation;
        let actor = match op.context.origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller.to_string()),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        let payload = json!({
            "operation_id": claim.id(),
            "reason": SUCCESSION_WAKES_REASON,
            "predecessor_session_id": predecessor,
            "successor_session_id": successor,
            "transferred_wake_job_ids": handed,
            "retired_wake_job_ids": retired,
        });
        self.conn.execute(
            "INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                op.project_id.to_string(),
                op.manager_session_id.to_string(),
                op.scope_version,
                actor,
                SUCCESSION_WAKES_EVENT,
                claim.id().to_string(),
                op.receipt.row_version + 1,
                serde_json::to_string(&payload)?,
                now()
            ],
        )?;
        tracing::info!(
            operation_id = %claim.id(),
            predecessor = %predecessor,
            carried = ?handed,
            retired = ?retired,
            "manager succession settled the displaced seat's resume wakes"
        );
        Ok(retired)
    }

    /// The wakes the publication of succession `operation` cancelled, for its
    /// succeeded receipt; `None` when it cancelled none.
    pub(crate) fn succession_cancelled_wakes(
        &self,
        operation: Uuid,
    ) -> Result<Option<ManagerCancelledWakesV1>> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_events WHERE kind=?1 AND record_key=?2",
                params![SUCCESSION_WAKES_EVENT, operation.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(payload) = payload else {
            return Ok(None);
        };
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        let job_ids: Vec<Uuid> = serde_json::from_value(payload["retired_wake_job_ids"].clone())?;
        if job_ids.is_empty() {
            return Ok(None);
        }
        Ok(Some(ManagerCancelledWakesV1 {
            reason: SUCCESSION_WAKES_REASON.to_string(),
            job_ids,
        }))
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
    ///   `new_seat` (#1587); so do the outcome wakes of settled jobs and every
    ///   predicate wait (`when`), whether or not a job is still live (#2471);
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
        let (mut watches, mut deploys, mut wakes) = (0, 0, 0);
        for old in lineage {
            let (w, d, k) = self.transfer_hop_pending_on(old, new_seat, false)?;
            watches += w;
            deploys += d;
            wakes += k;
        }
        Ok((watches, deploys, wakes))
    }

    /// #2021: a portfolio node's seat hands its seat to `new_seat`. Everything
    /// [`Self::transfer_displaced_seat_wakes_on`] hands a displaced project
    /// seat (terminal watches, daemon jobs and their batch waits, deploys and
    /// their outcome wakes, queue outcomes) moves from `old`, and so do the
    /// seat's own pending resume wakes (#2511): the node's next check-ins and its
    /// backstop belong to the seat that now runs it. Protected wakes (manager watches, open
    /// capacity incidents) keep their owners. Returns `(terminal watches,
    /// deploys, wakes)` retargeted, resume wakes included in the last.
    pub(crate) fn transfer_seat_pending_on(
        &self,
        old: Uuid,
        new_seat: Uuid,
    ) -> Result<(usize, usize, usize)> {
        if old == new_seat {
            return Ok((0, 0, 0));
        }
        self.transfer_hop_pending_on(old, new_seat, true)
    }

    /// What one superseded hop owns that its seat's successor takes over: the
    /// durable obligations of `manager_obligations` (#2471).
    fn transfer_hop_pending_on(
        &self,
        old: Uuid,
        new_seat: Uuid,
        backstop_follows: bool,
    ) -> Result<(usize, usize, usize)> {
        let moved = super::super::manager_obligations::transfer_manager_obligations_on(
            &self.conn,
            old,
            new_seat,
            backstop_follows,
        )?;
        Ok((moved.watches, moved.deploys, moved.wakes))
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

    /// #2021: a portfolio seat that hands its seat on takes its terminal
    /// watches and its own pending resume wakes with it; a protected manager
    /// watch and another session's wakes stay put.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_seat_handoff_moves_the_pending_watches_and_resume_wakes() {
        let store = Store::open_in_memory().unwrap();
        let old = session(&store, SessionStatus::Running, None);
        let new = session(&store, SessionStatus::Running, None);
        let other = session(&store, SessionStatus::Running, None);
        let resume = wake(&store, old, "resume");
        let watch = wake(&store, old, "on_terminal");
        let unrelated = wake(&store, other, "resume");
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        let (watches, deploys, wakes) = store.transfer_seat_pending_on(old, new).unwrap();
        tx.commit().unwrap();
        assert_eq!((watches, deploys, wakes), (1, 0, 1));
        let owner = |job: Uuid| {
            store
                .get_scheduled_job(&job)
                .unwrap()
                .unwrap()
                .wake_session_id
        };
        assert_eq!(owner(resume), Some(new));
        assert_eq!(owner(watch), Some(new));
        assert_eq!(owner(unrelated), Some(other));
        assert!(enabled(&store, resume) && enabled(&store, watch));
        // Nothing moves from a seat to itself.
        assert_eq!(store.transfer_seat_pending_on(new, new).unwrap(), (0, 0, 0));
    }

    /// #2389: after a portfolio self-handoff the successor lists the wakes
    /// that carried over (mode, predicate, deadline, a report waiting inside a
    /// wake) with the owner-scoped read, and nobody else's wakes appear.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn the_successor_seat_lists_the_wakes_that_carried_over() {
        use rsi_common::wake_predicate::{WakePredicate, WakeWhenState};

        let store = Store::open_in_memory().unwrap();
        let old = session(&store, SessionStatus::Running, None);
        let new = session(&store, SessionStatus::Running, None);
        let other = session(&store, SessionStatus::Running, None);
        let build = |owner: Uuid, mode: &str, name: &str, message: &str| {
            build_agent_scheduled_job(ScheduleWakeRequest {
                message: message.into(),
                in_seconds: Some(600),
                at: None,
                name: Some(name.into()),
                every_seconds: (mode == "resume").then_some(600),
                mode: Some(mode.into()),
                working_dir: std::path::PathBuf::from("/tmp/issue-2389"),
                provider: None,
                model: None,
                project_id: None,
                origin_session_id: Some(owner),
                watch_session_id: (mode == "on_terminal").then(Uuid::new_v4),
            })
            .unwrap()
        };
        let add = |job: &rsi_common::types::ScheduledJob| store.insert_scheduled_job(job).unwrap();

        let safety = build(old, "resume", "gm-safety-net", "check in");
        add(&safety);
        let watch = build(old, "on_terminal", "gm-watch-pm", "pm ended");
        add(&watch);
        let predicate = build(old, "resume", "gm-wait-landing", "landed?");
        let deadline = chrono::Utc::now() + chrono::Duration::seconds(3600);
        let sha = "0123456789abcdef0123456789abcdef01234567".to_string();
        store
            .insert_wake_when(
                &predicate,
                &WakeWhenState {
                    predicate: WakePredicate {
                        sha_on_rolling: Some(sha.clone()),
                        ..WakePredicate::default()
                    },
                    armed_at: chrono::Utc::now(),
                    deadline: Some(deadline),
                    repo_dir: None,
                    delivered_shas: Vec::new(),
                },
                false,
            )
            .unwrap();
        let long_report = "r".repeat(1000);
        let report = build(old, "resume", "Manager report", &long_report);
        add(&report);
        let delivered = build(old, "resume", "Manager notice", "already taken");
        add(&delivered);
        let message_row = |job: &rsi_common::types::ScheduledJob, state: &str, key: &str| {
            let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "INSERT INTO manager_tier_messages(id,direction,kind,source_ref,target_ref,
                        project_id,source_session_id,target_session_id,source_grant_version,
                        target_grant_version,body,body_digest,idempotency_key,state,
                        created_at,updated_at)
                     VALUES(?1,'up','report',?2,?3,NULL,?4,?5,NULL,NULL,'report',?6,?7,?8,?9,?9)",
                    rusqlite::params![
                        job.id.to_string(),
                        format!("project:{}", Uuid::new_v4()),
                        format!("portfolio:{}", Uuid::new_v4()),
                        Uuid::new_v4().to_string(),
                        old.to_string(),
                        "d".repeat(64),
                        key,
                        state,
                        stamp,
                    ],
                )
                .unwrap();
        };
        message_row(&report, "queued", "report-key");
        message_row(&delivered, "delivered", "notice-key");
        let foreign = build(other, "resume", "gm-safety-net", "not yours");
        add(&foreign);

        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        store.transfer_seat_pending_on(old, new).unwrap();
        tx.commit().unwrap();

        let listed = store.list_owned_wake_views(new, false, 64).unwrap();
        let by_name = |name: &str| {
            listed
                .iter()
                .find(|wake| wake.name == name)
                .unwrap_or_else(|| panic!("{name} carried over: {listed:?}"))
        };
        assert_eq!(listed.len(), 5, "{listed:?}");
        assert!(listed.iter().all(|wake| wake.enabled && !wake.protected));
        assert_eq!(by_name("gm-safety-net").job_id, safety.id);
        assert_eq!(by_name("gm-safety-net").mode, "resume");
        assert!(by_name("gm-safety-net").pending_message.is_none());
        assert_eq!(by_name("gm-watch-pm").job_id, watch.id);
        assert_eq!(by_name("gm-watch-pm").mode, "on_terminal");
        assert!(by_name("gm-watch-pm").watch_session_id.is_some());
        let when = by_name("gm-wait-landing");
        assert_eq!(when.mode, "when");
        assert_eq!(when.job_id, predicate.id);
        assert_eq!(when.when.as_ref().unwrap().sha_on_rolling, Some(sha));
        assert_eq!(when.deadline, Some(deadline));
        let waiting = by_name("Manager report").pending_message.as_ref().unwrap();
        assert_eq!(
            (waiting.kind.as_str(), waiting.state.as_str()),
            ("report", "queued")
        );
        assert_eq!(waiting.message_id, report.id);
        assert!(waiting.from.starts_with("project:"));
        assert_eq!(waiting.preview, "r".repeat(400));
        assert!(
            by_name("Manager notice").pending_message.is_none(),
            "a delivered message is not waiting"
        );

        // The handing seat keeps nothing; the foreign session's wake is its own.
        assert!(
            store
                .list_owned_wake_views(old, false, 64)
                .unwrap()
                .is_empty()
        );
        let others = store.list_owned_wake_views(other, false, 64).unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].job_id, foreign.id);
        assert!(listed.iter().all(|wake| wake.job_id != foreign.id));

        // A cancelled wake leaves the default list and returns with history.
        store
            .cancel_owned_scheduled_jobs(new, Some(safety.id), None)
            .unwrap();
        assert_eq!(
            store.list_owned_wake_views(new, false, 64).unwrap().len(),
            4
        );
        let all = store.list_owned_wake_views(new, true, 64).unwrap();
        assert_eq!(all.len(), 5);
        assert!(
            all.iter()
                .any(|wake| wake.job_id == safety.id && !wake.enabled)
        );
        // The bound is honoured.
        assert_eq!(store.list_owned_wake_views(new, true, 2).unwrap().len(), 2);
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
        let named_moved = watch_on(old, Uuid::new_v4());
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET name='inherited-watch' WHERE id=?1",
                [named_moved.to_string()],
            )
            .unwrap();

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

        // #1632: the seat that lists a transferred watch can cancel it, by id
        // and by name; a repeat is a named refusal, never a silent zero.
        let listed: Vec<Uuid> = store
            .list_owned_scheduled_jobs(successor.id, false, 64)
            .unwrap()
            .into_iter()
            .map(|(job, _)| job.id)
            .collect();
        assert!(listed.contains(&moved) && listed.contains(&named_moved));
        assert!(!listed.contains(&own_continuation));
        assert_eq!(
            store
                .cancel_owned_scheduled_jobs(successor.id, Some(moved), None)
                .unwrap(),
            vec![moved]
        );
        assert_eq!(
            store
                .cancel_owned_scheduled_jobs(successor.id, None, Some("inherited-watch"))
                .unwrap(),
            vec![named_moved]
        );
        assert!(!enabled(&store, moved) && !enabled(&store, named_moved));
        for error in [
            store
                .cancel_owned_scheduled_jobs(successor.id, Some(moved), None)
                .unwrap_err(),
            store
                .cancel_owned_scheduled_jobs(successor.id, None, Some("inherited-watch"))
                .unwrap_err(),
        ] {
            assert!(
                matches!(&error, crate::error::DaemonError::InvalidParam(code)
                    if code.starts_with("wake_already_disabled")),
                "{error:?}"
            );
        }
        // The displaced holder's own continuation stays out of the successor's reach.
        let foreign = store
            .cancel_owned_scheduled_jobs(successor.id, Some(own_continuation), None)
            .unwrap_err();
        assert!(
            matches!(&foreign, crate::error::DaemonError::InvalidParam(code)
                if code.starts_with("wake_not_found")),
            "{foreign:?}"
        );
        assert!(enabled(&store, own_continuation));
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
    fn a_successors_used_deploy_key_does_not_block_the_live_deploy_handover_1572() {
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
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: config.project_id,
                session_id: successor.id,
                epic_ids: Some(vec![lead.parent_id.unwrap()]),
                group_ids: Vec::new(),
                expected_row_version: config.row_version,
            })
            .unwrap();
        let held = store
            .get_harness_manager(config.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(held.manager_session_id, successor.id);
        let watch = store.get_scheduled_job(&watch).unwrap().unwrap();
        assert_eq!(watch.wake_session_id, Some(successor.id));
        // Both durable records survive: the live deploy settles under the new
        // seat, and the successor's earlier terminal record is untouched.
        let moved = store.get_agent_deploy(live.id).unwrap().unwrap();
        assert_eq!(moved.owner_session_id, Some(successor.id));
        assert_eq!(moved.state, DeployState::Staged);
        assert!(store.agent_deploy_interrupt(live.id).unwrap().requested);
        let terminal = store.get_agent_deploy(prior.id).unwrap().unwrap();
        assert_eq!(terminal.owner_session_id, Some(successor.id));
        assert_eq!(terminal.state, DeployState::Failed);
        // The key addresses each request in its own namespace: the new seat's
        // replay is its own settled record; a cancel by key reaches the live
        // deployment it inherited.
        let fingerprint = deploy_fingerprint_with(SHA, "x", 60, true);
        assert_eq!(
            store
                .replay_agent_deploy(successor.id, "reused-key", &fingerprint)
                .unwrap()
                .unwrap()
                .id,
            prior.id
        );
        assert_eq!(
            store
                .cancel_agent_deploy(successor.id, "reused-key", SHA, chrono::Utc::now())
                .unwrap()
                .id,
            live.id
        );
        assert_eq!(
            store.get_agent_deploy(live.id).unwrap().unwrap().state,
            DeployState::Failed
        );
        // The predecessor still replays the deployment it requested.
        assert_eq!(
            store
                .replay_agent_deploy(old, "reused-key", &fingerprint)
                .unwrap()
                .unwrap()
                .id,
            live.id
        );
    }
    /// #2471: a project manager's rotation hands every durable result and
    /// predicate obligation to the rotated seat in the rotation transaction,
    /// so maintenance finds nothing of them and each reaches the successor once.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_manager_rotation_carries_results_and_predicates_to_the_rotated_seat() {
        use crate::store::manager_coordinator::tests::fixture;
        use crate::store::manager_obligations::test_support as ob;
        use rsi_common::harness_manager_v2::ManagerPolicyV2;

        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let old = config.manager_session_id;
        let armed = ob::arm(
            &store,
            old,
            Some((config.project_id, lead.parent_id.unwrap())),
        );
        let mut rotated = store.get_session(old).unwrap().unwrap();
        rotated.id = Uuid::new_v4();
        rotated.continued_from = Some(old);
        rotated.rotation_depth += 1;
        store.insert_session(&rotated).unwrap();
        store
            .update_session_status(old, SessionStatus::Archived)
            .unwrap();
        assert!(
            store
                .record_harness_manager_rotation(old, rotated.id)
                .unwrap()
        );

        ob::assert_carried(&store, &armed, old, rotated.id, true);
        // Maintenance runs before delivery: the backstop followed the seat.
        let retired = store.sweep_superseded_resume_wakes().unwrap();
        assert!(!retired.contains(&armed.keepalive));
        ob::assert_maintenance_keeps_obligations(&store, &armed);
        ob::assert_second_transfer_is_empty(&store, old, rotated.id);
    }

    /// #2471: replacing the project manager carries the same obligations
    /// (the pre-existing watch, deploy and job transfer, plus outcome wakes
    /// and predicate waits) while the displaced holder keeps its own continuation.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_manager_replacement_carries_results_and_predicates_to_the_new_seat() {
        use crate::store::manager_coordinator::tests::fixture;
        use crate::store::manager_obligations::test_support as ob;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::ManagerPolicyV2;
        use rsi_common::types::SessionKind;

        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let old = config.manager_session_id;
        let armed = ob::arm(
            &store,
            old,
            Some((config.project_id, lead.parent_id.unwrap())),
        );
        let mut successor = crate::test_support::test_session(
            Uuid::new_v4(),
            std::path::PathBuf::from("/tmp/issue-2471"),
        );
        successor.session_kind = SessionKind::Standard;
        successor.project_id = Some(config.project_id);
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

        ob::assert_carried(&store, &armed, old, successor.id, false);
        let retired = store.sweep_superseded_resume_wakes().unwrap();
        assert!(!retired.iter().any(|id| {
            armed
                .obligations
                .iter()
                .any(|(_, obligation)| obligation == id)
        }));
        ob::assert_maintenance_keeps_obligations(&store, &armed);
        ob::assert_second_transfer_is_empty(&store, old, successor.id);
    }
}
