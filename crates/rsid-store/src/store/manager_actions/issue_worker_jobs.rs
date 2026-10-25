//! Completion evidence follows an explicit Issue-worker continuation (#584).
//! Job ownership, replay keys, source receipts and mutation authority stay put.

use super::{ACTION_KIND, ManagerActionOperationV2, ManagerFrozenSandboxSourceV1};
use crate::error::Result;
use crate::store::Store;
use rsi_common::agent_jobs::JobKind;
use rsi_common::harness_manager_v2::{ManagerActionStateV2, ManagerActionV2};
use rsi_common::types::{Session, SessionStatus};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

const MAX_HOPS: usize = 64;

impl Store {
    fn worker_job_launch(&self, worker: Uuid) -> Result<Option<ManagerActionOperationV2>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM harness_manager_v2_operations
             WHERE kind=?1 AND target_session_id=?2
               AND json_extract(payload_json,'$.request.operation.action')='create_session'
             ORDER BY rowid DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        match id.and_then(|id| Uuid::parse_str(&id).ok()) {
            Some(id) => self.manager_action_operation(id),
            None => Ok(None),
        }
    }

    /// Only the latest successfully published Issue launch may inherit reads.
    /// An admitted replacement (even one that later fails) supersedes it.
    /// Every edge must be journalled as continue_from for the same Issue,
    /// actually fork that predecessor's frozen source, and agree with the
    /// successor's verified custody allocation. A Session link alone grants
    /// nothing. This deliberately does not change manager succession rules.
    pub(crate) fn worker_job_predecessors(&self, caller: Uuid) -> Result<Vec<Session>> {
        let Some(first) = self.worker_job_launch(caller)? else {
            return Ok(Vec::new());
        };
        let Some(binding) = first.context.issue_binding.as_ref() else {
            return Ok(Vec::new());
        };
        let latest: bool = self.conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM harness_manager_v2_operations n
             JOIN harness_manager_v2_operations o ON o.id=?1
             WHERE n.kind=?2 AND n.rowid>o.rowid
               AND n.project_id=o.project_id
               AND json_extract(n.payload_json,'$.issue_binding.issue_id')=?3)",
            params![
                first.receipt.operation_id.to_string(),
                ACTION_KIND,
                binding.issue_id.to_string()
            ],
            |r| r.get(0),
        )?;
        if !latest {
            return Ok(Vec::new());
        }
        let project = first.project_id;
        let issue = binding.issue_id;
        let mut op = first;
        let mut current = caller;
        let mut seen = vec![caller];
        let mut predecessors = Vec::new();
        for _ in 0..MAX_HOPS {
            let Some(binding) = op.context.issue_binding.as_ref() else {
                return Ok(Vec::new());
            };
            if op.project_id != project
                || binding.issue_id != issue
                || op.receipt.state != ManagerActionStateV2::Succeeded
                || !matches!(
                    op.context.request.operation,
                    ManagerActionV2::CreateSession { .. }
                )
            {
                return Ok(Vec::new());
            }
            let Some(previous) = binding.continue_from else {
                return Ok(predecessors);
            };
            if seen.contains(&previous) {
                return Ok(Vec::new());
            }
            let (Some(session), Some(parent), Some(source)) = (
                self.get_session(current)?,
                self.get_session(previous)?,
                op.context.source.as_ref(),
            ) else {
                return Ok(Vec::new());
            };
            let custody = self.live_custody_for_session(current).ok();
            // A create's source session is its container. The frozen Path
            // selection, not that container's id/root, identifies the bytes
            // copied from the predecessor (#413's actual journal shape).
            let source_matches = matches!(&source.sandbox_source,
                Some(ManagerFrozenSandboxSourceV1::Path { path, .. })
                    if parent.sandbox_root.as_ref() == Some(path));
            if session.project_id != Some(project)
                || parent.project_id != Some(project)
                || session.continued_from != Some(previous)
                || matches!(
                    session.status,
                    SessionStatus::Archived | SessionStatus::Deleted
                )
                || !source_matches
                || source.working_dir != parent.working_dir
                || !custody.is_some_and(|c| {
                    c.source_commit == source.commit
                        && session.sandbox_root.as_deref()
                            == Some(std::path::Path::new(&c.sandbox_root))
                })
            {
                return Ok(Vec::new());
            }
            seen.push(previous);
            predecessors.push(parent);
            current = previous;
            let Some(previous_op) = self.worker_job_launch(previous)? else {
                return Ok(Vec::new());
            };
            op = previous_op;
        }
        Ok(Vec::new()) // overflow never grants a truncated lineage
    }

    /// Shared read/list/watch scope. No receipt is promoted to acceptance by
    /// this check: the recorded owner, cwd and exact-source bytes are untouched.
    pub(crate) fn worker_predecessor_job_observable(&self, caller: Uuid, id: Uuid) -> Result<bool> {
        let Some(row) = self.get_agent_job(id)? else {
            return Ok(false);
        };
        if !matches!(row.job.kind, JobKind::Test | JobKind::Build) {
            return Ok(false);
        }
        let project = self.agent_job_project(id)?;
        Ok(self.worker_job_predecessors(caller)?.iter().any(|s| {
            s.id == row.job.owner_session_id
                && s.project_id == project
                && s.sandbox_root.as_deref() == Some(std::path::Path::new(&row.job.cwd))
        }))
    }

    /// Carry only job completion obligations, in the successful launch's
    /// publication transaction. Never transfer jobs or their idempotency keys,
    /// arbitrary wakes, protected watches, or mutation permissions.
    pub(super) fn carry_worker_job_wakes_on(&self, successor: Uuid) -> Result<()> {
        if self.conn.is_autocommit() {
            return Err(crate::error::DaemonError::Store(
                "worker job wake transfer requires a publication transaction".into(),
            ));
        }
        for predecessor in self.worker_job_predecessors(successor)? {
            let wakes = self.list_owned_scheduled_jobs(predecessor.id, false, i64::MAX as usize)?;
            for wake in wakes.into_iter().map(|(w, _)| w).filter(|w| {
                w.enabled
                    && w.wake_session_id == Some(predecessor.id)
                    && w.wake_mode == rsi_common::types::WakeMode::Resume
            }) {
                let ids = if let Some(state) = self.wake_when_state(wake.id)? {
                    let Some(ids) = state.predicate.jobs_terminal else {
                        continue;
                    };
                    ids
                } else {
                    self.conn
                        .query_row(
                            "SELECT id FROM agent_jobs WHERE wake_job_id=?1",
                            [wake.id.to_string()],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()?
                        .and_then(|id| Uuid::parse_str(&id).ok())
                        .into_iter()
                        .collect()
                };
                if ids.is_empty() {
                    continue;
                }
                let mut allowed = true;
                for id in ids {
                    allowed &= self.worker_predecessor_job_observable(successor, id)?;
                }
                if !allowed {
                    continue;
                }
                self.conn.execute(
                    "UPDATE scheduled_jobs SET wake_session_id=?2,updated_at=?3,
                       next_fire_at=min(next_fire_at,?3),
                       schedule_json=json_set(json_remove(schedule_json,'$.continuation_retry'),
                         '$.self_wake_transfer',json_object('owner',?2,'event_sequence',
                           (SELECT coalesce(max(sequence),0) FROM conversation_events WHERE session_id=?2)))
                     WHERE id=?1 AND enabled=1 AND wake_session_id=?4
                       AND NOT EXISTS(SELECT 1 FROM harness_manager_watches WHERE job_id=?1)
                       AND NOT EXISTS(SELECT 1 FROM master_no_idle_capacity_incidents
                         WHERE state='open' AND (wake_job_id=?1 OR program_guard_job_id=?1))",
                    params![wake.id.to_string(), successor.to_string(), super::now(), predecessor.id.to_string()],
                )?;
            }
        }
        Ok(())
    }

    /// Future default-owner completions use the same proven publication path.
    /// The original owner remains the fallback on missing/unpublished custody.
    pub(crate) fn worker_job_completion_recipient(&self, owner: Uuid, job: Uuid) -> Result<Uuid> {
        let Some(op) = self.worker_job_launch(owner)? else {
            return Ok(owner);
        };
        let Some(binding) = op.context.issue_binding else {
            return Ok(owner);
        };
        let candidate: Option<String> = self
            .conn
            .query_row(
                "SELECT target_session_id FROM harness_manager_v2_operations
             WHERE kind=?1 AND project_id=?2
               AND json_extract(payload_json,'$.issue_binding.issue_id')=?3
             ORDER BY rowid DESC LIMIT 1",
                params![
                    ACTION_KIND,
                    op.project_id.to_string(),
                    binding.issue_id.to_string()
                ],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(candidate) = candidate {
            if let Ok(candidate) = Uuid::parse_str(&candidate)
                && self.worker_predecessor_job_observable(candidate, job)?
            {
                return Ok(candidate);
            }
        }
        Ok(owner)
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-03")
))]
mod tests;
