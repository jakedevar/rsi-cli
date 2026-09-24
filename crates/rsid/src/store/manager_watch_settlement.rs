//! Invocation-local manager notice witnesses; never part of an RPC or job DTO.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::types::ScheduledJob;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};

/// Match the existing enabled-watch bound per recipient. A planner may discover
/// later or differently rooted siblings; those remain armed without a witness.
const MAX_CAPTURED_MANAGER_WATCHES: usize = 64;

#[derive(Debug, Default)]
pub(crate) struct WatchFireCapture {
    manager_generations: HashMap<Uuid, i64>,
}

impl WatchFireCapture {
    pub(crate) fn is_manager_watch(&self, job_id: Uuid) -> bool {
        self.manager_generations.contains_key(&job_id)
    }

    /// Manager notice generation this capture observed for `job_id`.
    pub(crate) fn manager_generation(&self, job_id: Uuid) -> Option<i64> {
        self.manager_generations.get(&job_id).copied()
    }

    fn insert(&mut self, job_id: Uuid, generation: i64) -> Result<()> {
        if generation <= 0 {
            return Err(DaemonError::Store(
                "manager_watch_generation_invalid".into(),
            ));
        }
        self.manager_generations.insert(job_id, generation);
        Ok(())
    }
}

impl Store {
    /// Read the primary job and its manager cohort from one SQLite snapshot,
    /// before the scheduler awaits planning/delivery. Ownership comes only from
    /// the persisted binding. Capture the primary even if its envelope changed.
    pub(crate) fn capture_watch_fire(
        &self,
        job_id: Uuid,
    ) -> Result<Option<(ScheduledJob, WatchFireCapture)>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let Some(job) = self.get_scheduled_job(&job_id)? else {
            return Ok(None);
        };
        let mut capture = WatchFireCapture::default();
        let binding: Option<(i64, String, i64, String)> = self
            .conn
            .query_row(
                "SELECT notice_generation,project_id,scope_version,target_session_id
                 FROM harness_manager_watches WHERE job_id=?1",
                [job_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((generation, project, scope_version, target)) = binding {
            capture.insert(job_id, generation)?;
            // The scope index avoids scanning historical manager scopes. Only
            // enabled siblings for this bound recipient can consume the budget.
            let mut statement = self.conn.prepare(
                "SELECT w.job_id,w.notice_generation FROM harness_manager_watches w
                 JOIN scheduled_jobs j ON j.id=w.job_id
                 WHERE w.project_id=?1 AND w.scope_version=?2 AND w.target_session_id=?3
                   AND j.enabled=1 AND w.job_id<>?4
                 ORDER BY w.job_id LIMIT ?5",
            )?;
            let rows = statement.query_map(
                params![
                    project,
                    scope_version,
                    target,
                    job_id.to_string(),
                    (MAX_CAPTURED_MANAGER_WATCHES - 1) as i64,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            for row in rows {
                let (id, generation) = row?;
                let id =
                    Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string()))?;
                capture.insert(id, generation)?;
            }
        }
        tx.commit()?;
        Ok(Some((job, capture)))
    }

    /// Settle only the manager generation this invocation observed. The V103
    /// trigger advances the generation with every successful scheduled-row
    /// update, so rearm, operator edits and competing settlements all fence us.
    ///
    /// Missing/stale manager witnesses NEVER fall through to an unconditional
    /// update. The same statement preserves ordinary jobs' existing updates,
    /// including their enabled semantics. `None` leaves a timestamp untouched.
    pub(crate) fn settle_watch_fire(
        &self,
        capture: &WatchFireCapture,
        job_id: Uuid,
        last_fired_at: Option<&DateTime<Utc>>,
        next_fire_at: Option<&DateTime<Utc>>,
        enabled: bool,
    ) -> Result<bool> {
        self.settle_watch_fire_with_retry(
            capture,
            job_id,
            last_fired_at,
            next_fire_at,
            enabled,
            false,
        )
    }

    /// [`Self::settle_watch_fire`] that, with `clear_continuation_retry`,
    /// also removes the K2 `$.continuation_retry` state in the same UPDATE.
    /// Delivery and retry-exhaustion settlement use it so the terminal row
    /// write and the retry reset commit together: a crash can never leave an
    /// enabled job with no attempt history (review round 2
    /// `retry_exhaustion_crash_reset`).
    pub(crate) fn settle_watch_fire_with_retry(
        &self,
        capture: &WatchFireCapture,
        job_id: Uuid,
        last_fired_at: Option<&DateTime<Utc>>,
        next_fire_at: Option<&DateTime<Utc>>,
        enabled: bool,
        clear_continuation_retry: bool,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let generation = capture.manager_generations.get(&job_id).copied();
        let changed = self.update_watch_fire_row(
            generation,
            job_id,
            last_fired_at,
            next_fire_at,
            enabled,
            clear_continuation_retry,
        )?;
        if changed == 1 && generation.is_some() && enabled && last_fired_at.is_some() {
            let delivered = tx.execute(
                "UPDATE harness_manager_notices
                 SET delivered_at=COALESCE(delivered_at,?2)
                 WHERE id IN (
                    SELECT id FROM harness_manager_notices
                    WHERE job_id=?1 AND retired_at IS NULL
                      AND settled_at IS NULL AND delivered_at IS NULL
                    ORDER BY sequence LIMIT 16
                 )",
                params![
                    job_id.to_string(),
                    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
                ],
            )?;
            if delivered > 0 {
                // If more exact subjects remain, replace the prompt with the
                // next bounded tranche and re-arm it. Delivered-but-unread
                // subjects do not participate in another continuation. A
                // notice-free pre-V117 watch instead keeps last_fired_at and
                // remains armed for the legacy provider-output confirmation.
                self.refresh_manager_notice_job(job_id)?;
            }
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// The single generation-fenced scheduled-row update behind
    /// [`Self::settle_watch_fire`]. It opens no transaction, so a caller that
    /// already holds one (issue #648 atomic abandonment) shares it. With
    /// `clear_continuation_retry` the same UPDATE also removes the K2
    /// `$.continuation_retry` state, so a terminal settlement and its retry
    /// reset commit together (review round 2 `retry_exhaustion_crash_reset`).
    pub(super) fn update_watch_fire_row(
        &self,
        generation: Option<i64>,
        job_id: Uuid,
        last_fired_at: Option<&DateTime<Utc>>,
        next_fire_at: Option<&DateTime<Utc>>,
        enabled: bool,
        clear_continuation_retry: bool,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE scheduled_jobs
             SET last_fired_at=COALESCE(?1,last_fired_at),next_fire_at=COALESCE(?2,next_fire_at),
                 enabled=?3,updated_at=?4,
                 schedule_json=CASE WHEN ?7 AND json_valid(schedule_json)
                     THEN json_remove(schedule_json,'$.continuation_retry')
                     ELSE schedule_json END
             WHERE id=?5 AND (
                (?6 IS NOT NULL AND enabled=1 AND EXISTS(
                    SELECT 1 FROM harness_manager_watches
                    WHERE job_id=?5 AND notice_generation=?6))
                OR (?6 IS NULL AND NOT EXISTS(
                    SELECT 1 FROM harness_manager_watches WHERE job_id=?5)))",
            params![
                last_fired_at.map(|at| at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
                next_fire_at.map(|at| at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
                i32::from(enabled),
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                job_id.to_string(),
                generation,
                clear_continuation_retry,
            ],
        )?)
    }

    /// Issue #648 x #530: is `session` currently behind a RETRYABLE custody
    /// gate (a Prepared target reclaim on its live custody generation, or its
    /// custody root stripe held)? Exactly the gates whose refusal
    /// `is_retryable_custody_wake_error` maps to `CustodyUnavailable`. A
    /// session without live custody, or with a terminal custody refusal such
    /// as `cleanup_failed`, is not gated here.
    pub(crate) fn retryable_custody_gate(&self, session_id: Uuid) -> Result<bool> {
        let live: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT r.custody_id,r.generation FROM sandbox_custody_roots r
                 JOIN sessions s ON s.sandbox_custody_id=r.custody_id
                 WHERE s.id=?1 AND r.owner_session_id=?1 AND r.state='live'",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((custody_id, generation)) = live else {
            return Ok(false);
        };
        let prepared: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_target_reclaim_intents
             WHERE custody_id=?1 AND generation=?2 AND state='Prepared')",
            params![custody_id, generation],
            |row| row.get(0),
        )?;
        if prepared {
            return Ok(true);
        }
        let custody_id =
            Uuid::parse_str(&custody_id).map_err(|error| DaemonError::Store(error.to_string()))?;
        Ok(super::sandbox_custody::try_lock_custody_root(custody_id).is_none())
    }

    /// A Prepared reclaim or busy root stripe is a temporary custody refusal before delivery.
    /// Preserve the exact enabled job and watch generation, including Once
    /// jobs, without stamping last_fired_at or consuming its message.
    pub(crate) fn defer_retryable_custody_wake(
        &self,
        capture: &WatchFireCapture,
        observed: &ScheduledJob,
        retry_at: &DateTime<Utc>,
    ) -> Result<bool> {
        let generation = capture.manager_generations.get(&observed.id).copied();
        let changed = self.conn.execute(
            "UPDATE scheduled_jobs SET next_fire_at=?2,updated_at=?3
             WHERE id=?1 AND enabled=1 AND updated_at=?4 AND (
               (?5 IS NOT NULL AND EXISTS(
                 SELECT 1 FROM harness_manager_watches
                 WHERE job_id=?1 AND notice_generation=?5))
               OR (?5 IS NULL AND NOT EXISTS(
                 SELECT 1 FROM harness_manager_watches WHERE job_id=?1)))",
            params![
                observed.id.to_string(),
                retry_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                observed
                    .updated_at
                    .to_rfc3339_opts(SecondsFormat::Nanos, true),
                generation,
            ],
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::harness_manager::{
        AgentManagerInboxRequestV1, AgentManagerSendRequestV1, ConfigureHarnessManagerRequestV1,
        HarnessManagerMessageReceiptV1,
    };
    use rsi_common::types::{Project, SessionKind, SessionStatus, WakeMode};

    pub(crate) struct ManagerWatchFixture {
        pub(crate) manager: Uuid,
        pub(crate) epics: [Uuid; 3],
        pub(crate) leads: [Uuid; 3],
    }

    pub(crate) fn manager_watch_fixture(store: &Store) -> ManagerWatchFixture {
        let project = Uuid::new_v4();
        let now = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Manager notice settlement".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut owner = test_session(Uuid::new_v4(), "/tmp/manager-watch-cas".into());
        owner.project_id = Some(project);
        owner.session_kind = SessionKind::Standard;
        owner.status = SessionStatus::Completed;
        store.insert_session(&owner).unwrap();
        let mut group = owner.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let fixture = ManagerWatchFixture {
            manager: owner.id,
            epics: std::array::from_fn(|_| Uuid::new_v4()),
            leads: std::array::from_fn(|_| Uuid::new_v4()),
        };
        for index in 0..3 {
            let mut epic = owner.clone();
            epic.id = fixture.epics[index];
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group.id);
            store.insert_session(&epic).unwrap();
            let mut lead = owner.clone();
            lead.id = fixture.leads[index];
            lead.session_kind = SessionKind::Feature;
            lead.parent_id = Some(epic.id);
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
        }
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: owner.id,
                epic_ids: Some(fixture.epics.to_vec()),
                expected_row_version: 0,
            })
            .unwrap();
        fixture
    }

    impl ManagerWatchFixture {
        pub(crate) fn send(
            &self,
            store: &Store,
            index: usize,
            key: &str,
        ) -> HarnessManagerMessageReceiptV1 {
            store
                .manager_send(
                    self.manager,
                    &AgentManagerSendRequestV1 {
                        epic_id: self.epics[index],
                        message: format!("Readiness request {key}"),
                        idempotency_key: key.into(),
                    },
                )
                .unwrap()
        }

        pub(crate) fn notice(&self, store: &Store, index: usize, to_manager: bool) -> ScheduledJob {
            let (source, target) = if to_manager {
                (self.leads[index], self.manager)
            } else {
                (self.manager, self.leads[index])
            };
            store
                .list_scheduled_jobs()
                .unwrap()
                .into_iter()
                .find(|job| {
                    job.wake_mode == WakeMode::OnTerminal(source)
                        && job.wake_session_id == Some(target)
                        && store.is_harness_manager_watch(job.id).unwrap()
                })
                .unwrap()
        }
    }

    pub(crate) fn notice_state(store: &Store, job_id: Uuid) -> (serde_json::Value, i64, String) {
        let job = store.get_scheduled_job(&job_id).unwrap().unwrap();
        let (generation, signature) = store
            .conn
            .query_row(
                "SELECT notice_generation,attention_signature FROM harness_manager_watches WHERE job_id=?1",
                [job_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (serde_json::to_value(job).unwrap(), generation, signature)
    }

    #[test]
    fn manager_watch_capture_is_bounded_and_uses_binding_instead_of_name() {
        let store = Store::open_in_memory().unwrap();
        let fixture = manager_watch_fixture(&store);
        let primary = fixture.notice(&store, 0, true);
        let mut ordinary = primary.clone();
        ordinary.id = Uuid::new_v4();
        store.insert_scheduled_job(&ordinary).unwrap();
        let (_, ordinary_capture) = store.capture_watch_fire(ordinary.id).unwrap().unwrap();
        assert!(ordinary_capture.manager_generations.is_empty());
        let before = notice_state(&store, primary.id);
        assert!(
            !store
                .settle_watch_fire(
                    &ordinary_capture,
                    primary.id,
                    Some(&Utc::now()),
                    None,
                    false
                )
                .unwrap()
        );
        assert_eq!(notice_state(&store, primary.id), before);
        assert!(
            store
                .settle_watch_fire(
                    &ordinary_capture,
                    ordinary.id,
                    Some(&Utc::now()),
                    None,
                    true
                )
                .unwrap()
        );
        assert!(
            store
                .get_scheduled_job(&ordinary.id)
                .unwrap()
                .unwrap()
                .last_fired_at
                .is_some()
        );

        // Deliberately exceed admission's bound in a fixture to prove that a
        // damaged/legacy cohort still cannot create an unbounded capture.
        for _ in 0..70 {
            let mut sibling = primary.clone();
            sibling.id = Uuid::new_v4();
            store.insert_scheduled_job(&sibling).unwrap();
            store.conn.execute(
                "INSERT INTO harness_manager_watches(job_id,project_id,epic_id,scope_version,direction,source_session_id,target_session_id,attention_signature)
                 SELECT ?1,project_id,epic_id,scope_version,direction,source_session_id,target_session_id,attention_signature
                 FROM harness_manager_watches WHERE job_id=?2",
                params![sibling.id.to_string(), primary.id.to_string()],
            ).unwrap();
        }
        let (_, capture) = store.capture_watch_fire(primary.id).unwrap().unwrap();
        assert_eq!(
            capture.manager_generations.len(),
            MAX_CAPTURED_MANAGER_WATCHES
        );
        assert!(capture.is_manager_watch(primary.id));
        assert!(!capture.is_manager_watch(ordinary.id));
    }

    #[test]
    fn manager_watch_competing_settlement_and_operator_edit_invalidate_capture() {
        let store = Store::open_in_memory().unwrap();
        let fixture = manager_watch_fixture(&store);
        let primary = fixture.notice(&store, 0, true);
        let (_, first) = store.capture_watch_fire(primary.id).unwrap().unwrap();
        let (_, competing) = store.capture_watch_fire(primary.id).unwrap().unwrap();
        assert!(
            store
                .settle_watch_fire(&first, primary.id, Some(&Utc::now()), None, true)
                .unwrap()
        );
        let dispatched = notice_state(&store, primary.id);
        assert!(
            !store
                .settle_watch_fire(&competing, primary.id, Some(&Utc::now()), None, false)
                .unwrap()
        );
        assert_eq!(notice_state(&store, primary.id), dispatched);

        let (_, before_edit) = store.capture_watch_fire(primary.id).unwrap().unwrap();
        store
            .update_scheduled_job(
                &primary.id,
                &super::super::scheduled_jobs::ScheduledJobUpdate {
                    name: Some("Renamed manager notice".into()),
                    message: None,
                    schedule: None,
                    enabled: None,
                    next_fire_at: None,
                },
            )
            .unwrap();
        let edited = notice_state(&store, primary.id);
        assert_eq!(edited.1, dispatched.1 + 1);
        assert!(
            !store
                .settle_watch_fire(&before_edit, primary.id, Some(&Utc::now()), None, false)
                .unwrap()
        );
        assert_eq!(notice_state(&store, primary.id), edited);

        let (_, current) = store.capture_watch_fire(primary.id).unwrap().unwrap();
        assert!(
            store
                .settle_watch_fire(&current, primary.id, Some(&Utc::now()), None, false)
                .unwrap()
        );
        let disabled = notice_state(&store, primary.id);
        assert_eq!(disabled.1, edited.1 + 1);
        assert!(
            !store
                .settle_watch_fire(&current, primary.id, Some(&Utc::now()), None, true)
                .unwrap()
        );
        assert_eq!(notice_state(&store, primary.id), disabled);
    }

    #[test]
    fn notice_free_legacy_manager_watch_stays_armed_for_provider_confirmation() {
        let store = Store::open_in_memory().unwrap();
        let fixture = manager_watch_fixture(&store);
        let primary = fixture.notice(&store, 0, true);
        let mut legacy = primary.clone();
        legacy.id = Uuid::new_v4();
        legacy.name = "pre-V117 manager watch".into();
        store.insert_scheduled_job(&legacy).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_watches(job_id,project_id,epic_id,scope_version,direction,
                 source_session_id,target_session_id,attention_signature)
                 SELECT ?1,project_id,epic_id,scope_version,direction,
                        source_session_id,target_session_id,attention_signature
                 FROM harness_manager_watches WHERE job_id=?2",
                params![legacy.id.to_string(), primary.id.to_string()],
            )
            .unwrap();
        assert_eq!(store.manager_watch_delivery_state(legacy.id).unwrap(), None);

        let (_, capture) = store.capture_watch_fire(legacy.id).unwrap().unwrap();
        let fired_at = Utc::now();
        assert!(
            store
                .settle_watch_fire(&capture, legacy.id, Some(&fired_at), None, true)
                .unwrap()
        );
        let settled = store.get_scheduled_job(&legacy.id).unwrap().unwrap();
        assert!(settled.enabled);
        assert_eq!(settled.last_fired_at, Some(fired_at));
        assert_eq!(store.manager_watch_delivery_state(legacy.id).unwrap(), None);
    }

    #[test]
    fn delivered_notice_stays_armed_until_exact_inbox_retrieval_settles_it() {
        let store = Store::open_in_memory().unwrap();
        let fixture = manager_watch_fixture(&store);
        let primary = fixture.notice(&store, 0, true);
        let (_, capture) = store.capture_watch_fire(primary.id).unwrap().unwrap();
        let fired_at = Utc::now();
        assert!(
            store
                .settle_watch_fire(&capture, primary.id, Some(&fired_at), Some(&fired_at), true,)
                .unwrap()
        );
        let lifecycle: (Option<String>, Option<String>, Option<String>) = store
            .conn
            .query_row(
                "SELECT delivered_at,retrieved_at,settled_at
                 FROM harness_manager_notices WHERE job_id=?1 ORDER BY sequence LIMIT 1",
                [primary.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(lifecycle.0.is_some());
        assert_eq!(lifecycle.1, None);
        assert_eq!(lifecycle.2, None);
        assert!(
            store
                .get_scheduled_job(&primary.id)
                .unwrap()
                .unwrap()
                .enabled
        );

        let inbox = store
            .manager_inbox(fixture.manager, &AgentManagerInboxRequestV1::default())
            .unwrap();
        assert!(inbox.notices.iter().any(|notice| {
            notice.epic_id == Some(fixture.epics[0]) && notice.delivered_at.is_some()
        }));
        assert!(
            !store
                .get_scheduled_job(&primary.id)
                .unwrap()
                .unwrap()
                .enabled,
            "retrieval disables only the fully settled transport job"
        );
    }

    #[test]
    fn delivery_marks_only_the_exact_rendered_notice_tranche() {
        let store = Store::open_in_memory().unwrap();
        let fixture = manager_watch_fixture(&store);
        let receipts: Vec<_> = (0..18)
            .map(|index| fixture.send(&store, 0, &format!("tranche-{index}")))
            .collect();
        let job = fixture.notice(&store, 0, false);
        let (_, capture) = store.capture_watch_fire(job.id).unwrap().unwrap();
        let fired_at = Utc::now();
        assert!(
            store
                .settle_watch_fire(&capture, job.id, Some(&fired_at), Some(&fired_at), true)
                .unwrap()
        );
        let lifecycle: (i64, i64) = store
            .conn
            .query_row(
                "SELECT sum(delivered_at IS NOT NULL),sum(delivered_at IS NULL)
                 FROM harness_manager_notices
                 WHERE job_id=?1 AND settled_at IS NULL",
                [job.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, (16, 2));
        let rearmed = store.get_scheduled_job(&job.id).unwrap().unwrap();
        assert!(rearmed.enabled);
        assert_eq!(rearmed.last_fired_at, None);
        for receipt in &receipts[16..] {
            assert!(rearmed.message.contains(&receipt.message_id.to_string()));
        }
        for receipt in &receipts[..16] {
            let delivered: bool = store
                .conn
                .query_row(
                    "SELECT delivered_at IS NOT NULL FROM harness_manager_notices
                     WHERE kind='message' AND subject_id=?1",
                    [receipt.message_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(delivered);
        }
    }
}
