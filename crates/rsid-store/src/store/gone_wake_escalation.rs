//! Durable escalation for a terminal result wake whose target is gone (#2451).
//!
//! A wake armed for a producer's result (an agent job settling, a merge-queue
//! entry settling, any other `Resume` wake) targets one session. When that
//! session no longer exists at fire time the scheduler used to log the
//! refusal, broadcast a bus message and retire the one-shot, so the outcome
//! reached no one. [`Store::escalate_gone_wake_target`] gives the outcome a
//! durable owner without resuming the deleted session or widening authority:
//!
//! 1. the project's current manager seat (the project is derived from the
//!    producer row, the wake row or the gone target's remaining row) receives
//!    one ordinary `Resume` wake whose id is derived from the original wake id,
//!    so a scheduler restart or a repeated fire inserts nothing twice;
//! 2. with no resolvable manager, one operator-scope to-do keyed by the wake
//!    id retains the same facts for the operator.
//!
//! The escalation names the wake id, the original target, the producer
//! job/entry id and the refusal.

use chrono::{DateTime, Utc};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, SessionStatus, WakeMode};
use rusqlite::OptionalExtension;
use uuid::Uuid;

use super::Store;
use super::scheduled_jobs::{get_scheduled_job_conn, insert_scheduled_job_conn};
use crate::error::Result;

const ESCALATION_NAMESPACE: Uuid = Uuid::from_u128(0x2451_9a3c_6d1e_5f07_8b42_c0de_7a15_e5ca);
/// Original-message bytes carried into the escalation wake.
const MAX_ORIGINAL_MESSAGE_BYTES: usize = 6000;
/// Priority of the operator to-do (urgent: a terminal outcome has no reader).
const TODO_PRIORITY: u8 = 2;

/// Which durable owner received the escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoneWakeEscalation {
    /// The project's current manager seat holds the escalation wake.
    Manager {
        project_id: Uuid,
        seat: Uuid,
        escalation_wake_id: Uuid,
        /// `false`: the escalation already existed (replay).
        created: bool,
    },
    /// No manager resolved; the operator to-do list holds the notice.
    OperatorTodo {
        todo_id: Uuid,
        project_id: Option<Uuid>,
        created: bool,
    },
}

/// The deterministic id of the escalation wake for `original_wake_id`.
pub fn gone_wake_escalation_id(original_wake_id: Uuid) -> Uuid {
    Uuid::new_v5(&ESCALATION_NAMESPACE, original_wake_id.as_bytes())
}

/// What the store knows about the producer of a wake.
#[derive(Default)]
struct Producer {
    /// `agent job <id> ...` or `merge-queue entry <id> ...`.
    description: Option<String>,
    project_id: Option<Uuid>,
    /// The session the producer recorded as owner or source.
    origin_session_id: Option<Uuid>,
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &text[..end])
}

fn parse_opt_uuid(text: Option<String>) -> Option<Uuid> {
    text.and_then(|text| Uuid::parse_str(&text).ok())
}

impl Store {
    fn gone_wake_producer(&self, wake_id: Uuid) -> Result<Producer> {
        let wake = wake_id.to_string();
        let job: Option<(String, String, Option<String>, String, String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT id, owner_session_id, project_id, kind, state, exit_code \
                 FROM agent_jobs WHERE wake_job_id=?1",
                [&wake],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        if let Some((id, owner, project, kind, state, exit_code)) = job {
            return Ok(Producer {
                description: Some(format!(
                    "agent job {id} (kind {kind}, state {state}, exit_code {})",
                    exit_code.map_or_else(|| "none".to_string(), |code| code.to_string())
                )),
                project_id: parse_opt_uuid(project),
                origin_session_id: Uuid::parse_str(&owner).ok(),
            });
        }
        let entry: Option<(String, Option<String>, String, String)> = self
            .conn
            .query_row(
                "SELECT id, project_id, source_session_id, state \
                 FROM rolling_queue_entries WHERE wake_job_id=?1",
                [&wake],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((id, project, source, state)) = entry {
            return Ok(Producer {
                description: Some(format!("merge-queue entry {id} (state {state})")),
                project_id: parse_opt_uuid(project),
                origin_session_id: Uuid::parse_str(&source).ok(),
            });
        }
        Ok(Producer::default())
    }

    /// The current manager seat of the first of `projects` that has one.
    /// A seat counts only while its session row still exists and is not
    /// deleted; the escalation resumes that seat, never the gone target.
    fn current_manager_seat(&self, projects: &[Uuid]) -> Result<Option<(Uuid, Uuid)>> {
        for project in projects {
            let Some(config) = self.get_harness_manager_notice_config(*project)? else {
                continue;
            };
            let Some(seat) = config.current_session_id else {
                continue;
            };
            if self
                .get_session(seat)?
                .is_some_and(|session| session.status != SessionStatus::Deleted)
            {
                return Ok(Some((*project, seat)));
            }
        }
        Ok(None)
    }

    /// Give a wake whose target is gone one durable escalation. Idempotent per
    /// original wake id: a repeated fire or a scheduler restart before the
    /// one-shot is settled inserts nothing a second time.
    ///
    /// Never resumes `job.wake_session_id` and never grants authority: the
    /// escalation is an ordinary `Resume` wake to the project's current
    /// manager seat, or an operator-scope to-do.
    ///
    /// # Errors
    /// A persistence error, or `operator_todo_limit` when there is no manager
    /// and the operator list is full.
    pub fn escalate_gone_wake_target(
        &self,
        job: &ScheduledJob,
        refusal: &str,
        now: DateTime<Utc>,
    ) -> Result<GoneWakeEscalation> {
        self.escalate_wake(job, refusal, now, true)
    }

    /// #2477: give a result wake whose delivery kept being refused (the retry
    /// budget ran out while its target still exists) the same durable owner as
    /// a gone target, so settling the one-shot never loses the outcome.
    ///
    /// # Errors
    /// As [`Self::escalate_gone_wake_target`].
    pub fn escalate_undelivered_wake(
        &self,
        job: &ScheduledJob,
        refusal: &str,
        now: DateTime<Utc>,
    ) -> Result<GoneWakeEscalation> {
        self.escalate_wake(job, refusal, now, false)
    }

    fn escalate_wake(
        &self,
        job: &ScheduledJob,
        refusal: &str,
        now: DateTime<Utc>,
        target_gone: bool,
    ) -> Result<GoneWakeEscalation> {
        let target = job.wake_session_id;
        let producer = self.gone_wake_producer(job.id)?;
        let mut projects: Vec<Uuid> = Vec::new();
        let mut note = |project: Option<Uuid>| {
            if let Some(project) = project
                && !projects.contains(&project)
            {
                projects.push(project);
            }
        };
        note(producer.project_id);
        note(job.project_id);
        for session in [target, producer.origin_session_id].into_iter().flatten() {
            note(self.get_session(session)?.and_then(|row| row.project_id));
        }
        let why = if target_gone {
            "is gone"
        } else {
            "could not accept delivery; recovery details follow"
        };
        let facts = format!(
            "wake {} ({}) could not be delivered: original target {} {why} ({refusal}); \
             producer {}",
            job.id,
            job.name,
            target.map_or_else(|| "none".to_string(), |id| id.to_string()),
            producer
                .description
                .as_deref()
                .unwrap_or("not recorded for this wake"),
        );
        let seat = self.current_manager_seat(&projects)?;
        let Some((project_id, seat)) = seat else {
            return self.gone_wake_operator_todo(job, &facts, projects.first().copied());
        };
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let escalation = {
            let escalation_wake_id = gone_wake_escalation_id(job.id);
            let created = get_scheduled_job_conn(&tx, &escalation_wake_id)?.is_none();
            if created {
                insert_scheduled_job_conn(
                    &tx,
                    &ScheduledJob {
                        id: escalation_wake_id,
                        name: format!("gone-wake-escalation:{}", job.id),
                        message: format!(
                            "[wake-escalation] {facts}. The original target is not resumed; \
                             you are the project's current manager. Original message:\n{}",
                            clip(&job.message, MAX_ORIGINAL_MESSAGE_BYTES)
                        ),
                        schedule: ScheduleSpec {
                            recurrence: Recurrence::Once,
                            anchor: now,
                        },
                        last_fired_at: None,
                        next_fire_at: now,
                        enabled: true,
                        working_dir: None,
                        provider: None,
                        model: None,
                        project_id: None,
                        created_at: now,
                        updated_at: now,
                        wake_mode: WakeMode::Resume,
                        wake_session_id: Some(seat),
                    },
                )?;
            }
            GoneWakeEscalation::Manager {
                project_id,
                seat,
                escalation_wake_id,
                created,
            }
        };
        tx.commit()?;
        Ok(escalation)
    }

    /// No manager resolved: one operator-scope to-do keyed by the wake id.
    fn gone_wake_operator_todo(
        &self,
        job: &ScheduledJob,
        facts: &str,
        project: Option<Uuid>,
    ) -> Result<GoneWakeEscalation> {
        let write = self.operator_todo_system_notice(
            &format!("gone-wake:{}", job.id),
            project,
            &clip(
                &format!("Undelivered result wake {} (target gone)", job.id),
                480,
            ),
            &clip(
                &format!(
                    "{facts}. No current manager resolved; inspect the producer's outcome and \
                     close this item with evidence. Original message: {}",
                    job.message.replace('\n', " ")
                ),
                2000,
            ),
            TODO_PRIORITY,
        )?;
        Ok(GoneWakeEscalation::OperatorTodo {
            todo_id: write.todo.id,
            project_id: write.todo.project_id,
            created: write.created,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_jobs::NewAgentJob;
    use crate::store::manager_coordinator::tests::fixture;
    use rsi_common::agent_jobs::{
        AgentJobResultV1, BuildCommand, BuildJobParams, JobParams, JobState, JobWake,
    };
    use rsi_common::rolling_queue::{
        RollingQueueBinding, RollingQueueEntryState, RollingQueueOutcome,
    };
    use rsi_common::types::{Session, SessionStatus};

    use crate::store::rolling_queue::NewQueueEntry;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn escalations(store: &Store) -> Vec<ScheduledJob> {
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("gone-wake-escalation:"))
            .collect()
    }

    /// Settle a failed build job owned by `owner` with a wake; returns the
    /// job id and the owner's wake row.
    fn failed_job_wake(store: &Store, owner: Uuid, project: Option<Uuid>) -> (Uuid, ScheduledJob) {
        let id = Uuid::new_v4();
        let new = NewAgentJob {
            id,
            owner_session_id: owner,
            project_id: project,
            name: Some("check".into()),
            params: JobParams::Build(BuildJobParams {
                command: BuildCommand::Check,
                package: None,
                workspace: true,
                all_targets: false,
                release: false,
                no_default_features: false,
                features: Vec::new(),
                shards: Vec::new(),
                cpu_quota_percent: None,
                sha: None,
            }),
            cwd: "/tmp/sandbox".into(),
            unit_name: format!("rsi-job-{id}"),
            log_path: format!("/tmp/jobs/{id}.log"),
            status_path: format!("/tmp/jobs/{id}.status"),
            idempotency_key: None,
            wake: JobWake::Owner,
        };
        store.insert_agent_job(&new, Utc::now()).unwrap();
        let result = AgentJobResultV1 {
            exit_code: Some(101),
            ..AgentJobResultV1::default()
        };
        let wake = store
            .settle_agent_job(id, JobState::Failed, &result, true, Utc::now())
            .unwrap()
            .expect("a failed job with wake writes its owner wake");
        (id, store.get_scheduled_job(&wake).unwrap().unwrap())
    }

    fn refused_landing_wake(store: &Store, source: Uuid, project: Uuid) -> (Uuid, ScheduledJob) {
        let (entry, _) = store
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: Some(project),
                    repo_path: "/tmp/repo".into(),
                    source_commit: "a".repeat(40),
                    source_session_id: source,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: Vec::new(),
                    test_filters: Vec::new(),
                    fixes_tests: Vec::new(),
                    idempotency_key: "gone-target".into(),
                },
                Utc::now(),
            )
            .unwrap();
        store
            .claim_next_rolling_queue_entry(Utc::now())
            .unwrap()
            .unwrap();
        store
            .settle_rolling_queue_entry(
                entry.id,
                RollingQueueEntryState::Refused,
                &RollingQueueOutcome {
                    refusal: Some("gate_failed".into()),
                    ..RollingQueueOutcome::default()
                },
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        let wake = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.name.starts_with("merge-queue-"))
            .expect("the refusal writes a wake");
        (entry.id, wake)
    }

    fn assert_escalated_to(
        store: &Store,
        seat: Uuid,
        wake: &ScheduledJob,
        target: Uuid,
        producer: Uuid,
    ) {
        let all = escalations(store);
        assert_eq!(all.len(), 1, "exactly one durable escalation");
        let escalation = &all[0];
        assert_eq!(escalation.wake_session_id, Some(seat));
        assert_ne!(escalation.wake_session_id, Some(target));
        assert!(escalation.enabled, "the escalation is armed for the seat");
        for fact in [
            wake.id.to_string(),
            target.to_string(),
            producer.to_string(),
            "Session not found".to_string(),
        ] {
            assert!(
                escalation.message.contains(&fact),
                "escalation names {fact}: {}",
                escalation.message
            );
        }
        assert!(
            escalation.message.contains(&wake.message),
            "the original outcome rides along"
        );
    }

    fn refusal(target: Uuid) -> String {
        crate::error::DaemonError::SessionNotFound(target).to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn failed_job_wake_for_a_deleted_owner_escalates_once_to_the_current_manager() {
        let store = store();
        let (config, lead) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let mut owner: Session = lead.clone();
        owner.id = Uuid::new_v4();
        store.insert_session(&owner).unwrap();
        let (job_id, wake) = failed_job_wake(&store, owner.id, Some(config.project_id));
        store.purge_session(owner.id).unwrap();
        assert!(store.get_session(owner.id).unwrap().is_none());

        let text = refusal(owner.id);
        let first = store
            .escalate_gone_wake_target(&wake, &text, Utc::now())
            .unwrap();
        assert!(matches!(
            first,
            GoneWakeEscalation::Manager { seat: s, created: true, .. } if s == seat
        ));
        // Repeated fire and scheduler restart: the same original row again.
        for _ in 0..2 {
            let again = store
                .escalate_gone_wake_target(&wake, &text, Utc::now())
                .unwrap();
            assert!(matches!(
                again,
                GoneWakeEscalation::Manager { created: false, .. }
            ));
        }
        assert_escalated_to(&store, seat, &wake, owner.id, job_id);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn refused_landing_wake_for_a_removed_source_escalates_once_to_the_current_manager() {
        let store = store();
        let (config, lead) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let mut source: Session = lead.clone();
        source.id = Uuid::new_v4();
        store.insert_session(&source).unwrap();
        let (entry_id, wake) = refused_landing_wake(&store, source.id, config.project_id);
        assert_eq!(wake.wake_session_id, Some(source.id));
        store.purge_session(source.id).unwrap();

        let text = refusal(source.id);
        for _ in 0..3 {
            store
                .escalate_gone_wake_target(&wake, &text, Utc::now())
                .unwrap();
        }
        assert_escalated_to(&store, seat, &wake, source.id, entry_id);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn ordinary_gone_target_wake_resolves_the_project_from_the_wake_row() {
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let target = Uuid::new_v4();
        let wake = ScheduledJob {
            id: Uuid::new_v4(),
            name: "plain-wake".into(),
            message: "check the deploy".into(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: Utc::now(),
            },
            last_fired_at: None,
            next_fire_at: Utc::now(),
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: Some(config.project_id),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(target),
        };
        store.insert_scheduled_job(&wake).unwrap();
        let text = refusal(target);
        store
            .escalate_gone_wake_target(&wake, &text, Utc::now())
            .unwrap();
        store
            .escalate_gone_wake_target(&wake, &text, Utc::now())
            .unwrap();
        let all = escalations(&store);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].wake_session_id, Some(seat));
        assert!(all[0].message.contains(&wake.id.to_string()));
        assert!(all[0].message.contains(&target.to_string()));
        assert!(all[0].message.contains("check the deploy"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn no_resolvable_manager_retains_one_operator_todo() {
        let store = store();
        let target = Uuid::new_v4();
        let (job_id, wake) = failed_job_wake(&store, target, None);
        let text = refusal(target);
        let first = store
            .escalate_gone_wake_target(&wake, &text, Utc::now())
            .unwrap();
        let GoneWakeEscalation::OperatorTodo {
            todo_id,
            created: true,
            ..
        } = first
        else {
            panic!("no manager resolves, so the operator list holds the notice: {first:?}");
        };
        let again = store
            .escalate_gone_wake_target(&wake, &text, Utc::now())
            .unwrap();
        assert!(matches!(
            again,
            GoneWakeEscalation::OperatorTodo { created: false, todo_id: id, .. } if id == todo_id
        ));
        assert!(escalations(&store).is_empty(), "no seat wake is invented");
        let todo = store.operator_todo_by_id(todo_id).unwrap().unwrap();
        for fact in [wake.id.to_string(), target.to_string(), job_id.to_string()] {
            assert!(todo.how.contains(&fact), "todo names {fact}: {}", todo.how);
        }
        assert!(todo.how.contains("Session not found"));
        assert_eq!(store.operator_todo_list(None).unwrap().open.len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_deleted_manager_seat_falls_back_to_the_operator_todo() {
        let store = store();
        let (config, _) = fixture(&store, Default::default());
        let seat = config.current_session_id.unwrap();
        let target = Uuid::new_v4();
        let (_, wake) = failed_job_wake(&store, target, Some(config.project_id));
        store
            .update_session_status(seat, SessionStatus::Deleted)
            .unwrap();
        let outcome = store
            .escalate_gone_wake_target(&wake, &refusal(target), Utc::now())
            .unwrap();
        assert!(matches!(
            outcome,
            GoneWakeEscalation::OperatorTodo { project_id: Some(p), .. } if p == config.project_id
        ));
        assert!(escalations(&store).is_empty());
    }
}
