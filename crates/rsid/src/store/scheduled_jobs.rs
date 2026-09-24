use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use rsi_common::program_runs::ProgramRunActionV1;
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};

use super::Store;
use super::row_mappers::{parse_timestamp, session_provider_to_str, str_to_session_provider};
use crate::error::{DaemonError, Result};

/// Persist the meaning of a watch transition independently of the mutable
/// scheduled-job row. Only launched automatic children have a repair witness.
fn record_agent_child_watch_transition(
    conn: &Connection,
    job_id: &Uuid,
    state: &str,
    at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_child_watch_witness
             (owner_session_id,child_session_id,job_id,state,updated_at)
         SELECT request.owner_session_id,request.child_session_id,job.id,?2,?3
           FROM scheduled_jobs AS job
           JOIN agent_spawn_requests AS request
             ON request.owner_session_id=job.wake_session_id
            AND job.wake_mode='on_terminal:' || request.child_session_id
          WHERE job.id=?1 AND request.state='launched'
            AND NOT EXISTS (
                SELECT 1 FROM harness_manager_watches AS manager_watch
                WHERE manager_watch.job_id=job.id)
         ON CONFLICT(owner_session_id,child_session_id)
         DO UPDATE SET job_id=excluded.job_id,state=excluded.state,updated_at=excluded.updated_at",
        params![job_id.to_string(), state, at],
    )?;
    Ok(())
}

/// Mutable-field update payload for `update_scheduled_job()`.
pub struct ScheduledJobUpdate {
    pub name: Option<String>,
    pub message: Option<String>,
    pub schedule: Option<ScheduleSpec>,
    pub enabled: Option<bool>,
    pub next_fire_at: Option<DateTime<Utc>>,
}

impl Store {
    pub(crate) fn insert_or_replay_program_run_wake_job(
        &self,
        action: &ProgramRunActionV1,
        project_id: Uuid,
    ) -> Result<(ScheduledJob, bool)> {
        let expected = ScheduledJob {
            id: action.id,
            name: format!("ProgramRun wake {}", action.program_run_id),
            message: format!(
                "Resume ProgramRun {} action {}",
                action.program_run_id, action.id
            ),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: action.not_before,
            },
            last_fired_at: None,
            next_fire_at: action.not_before,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: Some(project_id),
            created_at: action.created_at,
            updated_at: action.created_at,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(action.controller_session_id),
        };
        if let Some(existing) = self.get_scheduled_job(&action.id)? {
            let envelope = |job: &ScheduledJob| {
                serde_json::to_value(serde_json::json!({
                    "id": job.id,
                    "name": job.name,
                    "message": job.message,
                    "schedule": job.schedule,
                    "working_dir": job.working_dir,
                    "provider": job.provider,
                    "model": job.model,
                    "project_id": job.project_id,
                    "created_at": job.created_at,
                    "wake_mode": job.wake_mode,
                    "wake_session_id": job.wake_session_id,
                }))
                .map_err(|error| DaemonError::Store(error.to_string()))
            };
            let existing_json = envelope(&existing)?;
            let expected_json = envelope(&expected)?;
            if existing_json != expected_json {
                return Err(DaemonError::Store(
                    "program_run_downstream_replay_conflict".into(),
                ));
            }
            return Ok((existing, true));
        }
        self.insert_scheduled_job(&expected)?;
        Ok((expected, false))
    }

    pub fn insert_scheduled_job(&self, job: &ScheduledJob) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        insert_scheduled_job_conn(&tx, job)?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_scheduled_jobs(&self) -> Result<Vec<ScheduledJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                    enabled, working_dir, provider, model, project_id, created_at, updated_at,
                    wake_mode, wake_session_id
             FROM scheduled_jobs ORDER BY next_fire_at ASC",
        )?;
        let jobs = stmt
            .query_map([], |row| Ok(map_scheduled_job_row(row)))?
            .filter_map(|r| keep_readable_row("list_scheduled_jobs", r))
            .collect();
        Ok(jobs)
    }

    /// Scan only currently enabled terminal watches. The existing partial index
    /// excludes the unbounded disabled history retained for audit and repair.
    pub(crate) fn list_enabled_terminal_watches(&self) -> Result<Vec<ScheduledJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                    enabled, working_dir, provider, model, project_id, created_at, updated_at,
                    wake_mode, wake_session_id
             FROM scheduled_jobs
             WHERE enabled=1 AND wake_mode LIKE 'on_terminal:%'
             ORDER BY next_fire_at ASC",
        )?;
        let jobs = stmt
            .query_map([], |row| Ok(map_scheduled_job_row(row)))?
            .filter_map(|r| keep_readable_row("list_enabled_terminal_watches", r))
            .collect();
        Ok(jobs)
    }

    pub fn get_scheduled_job(&self, id: &Uuid) -> Result<Option<ScheduledJob>> {
        get_scheduled_job_conn(&self.conn, id)
    }

    /// Exact primary-key existence read that remains true for an unreadable
    /// row. Terminal safety code uses this to distinguish absence from a
    /// malformed deterministic sentinel without scanning historical jobs.
    pub(crate) fn scheduled_job_exists(&self, id: &Uuid) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id = ?1)",
                params![id.to_string()],
                |row| row.get(0),
            )
            .map_err(DaemonError::Database)
    }

    /// Resolve daemon-owned program identity from the deterministic job id,
    /// independently of every mutable field in the scheduled-job row.
    ///
    /// Ordinary scheduled jobs are v4 UUIDs, so they avoid the session scan.
    /// Program guards are v5 UUIDs derived from their owning session. Checking
    /// every durable session row makes a malformed legacy guard recognizable
    /// even when its wake mode, wake target, recurrence, anchor, or due slot
    /// has been changed.
    pub(crate) fn program_guard_owner_for_job_id(&self, id: &Uuid) -> Result<Option<Uuid>> {
        if id.get_version_num() != 5 {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare("SELECT id FROM sessions")?;
        let session_ids = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for raw_session_id in session_ids {
            let raw_session_id = raw_session_id?;
            let session_id = Uuid::parse_str(&raw_session_id).map_err(|error| {
                DaemonError::Store(format!(
                    "invalid session UUID while resolving program guard ownership: {error}"
                ))
            })?;
            if crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(
                session_id,
            ) == *id
            {
                return Ok(Some(session_id));
            }
        }
        Ok(None)
    }

    /// Restore an existing deterministic program-guard row to the exact
    /// daemon-derived envelope. The authenticated registration path validates
    /// `job` before calling this method. Updating in place preserves the stable
    /// primary-key identity and repairs even rows that typed reads cannot map.
    pub(crate) fn restore_program_guard_scheduled_job(&self, job: &ScheduledJob) -> Result<()> {
        let schedule_json =
            serde_json::to_string(&job.schedule).map_err(|e| DaemonError::Store(e.to_string()))?;
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE scheduled_jobs
             SET name=?1,message=?2,schedule_json=?3,last_fired_at=?4,next_fire_at=?5,
                 enabled=?6,working_dir=?7,provider=?8,model=?9,project_id=?10,
                 created_at=?11,updated_at=?12,wake_mode=?13,wake_session_id=?14
             WHERE id=?15",
            params![
                job.name,
                job.message,
                schedule_json,
                job.last_fired_at
                    .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
                job.next_fire_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                i32::from(job.enabled),
                job.working_dir
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                job.provider.map(session_provider_to_str),
                job.model,
                job.project_id.map(|project_id| project_id.to_string()),
                job.created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                job.updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                wake_mode_string(job.wake_mode),
                job.wake_session_id.map(|session_id| session_id.to_string()),
                job.id.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "master_program_guard_missing_during_restore".into(),
            ));
        }
        super::source_worktree_v120::refresh_scheduled_job_path_projection(
            &tx,
            &job.id.to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Find all enabled jobs whose next_fire_at is <= now.
    pub fn list_due_scheduled_jobs(&self, now: &DateTime<Utc>) -> Result<Vec<ScheduledJob>> {
        let now_str = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut stmt = self.conn.prepare(
            "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                    enabled, working_dir, provider, model, project_id, created_at, updated_at,
                    wake_mode, wake_session_id
             FROM scheduled_jobs
             WHERE enabled = 1 AND next_fire_at <= ?1
             ORDER BY next_fire_at ASC",
        )?;
        let jobs = stmt
            .query_map(params![now_str], |row| Ok(map_scheduled_job_row(row)))?
            .filter_map(|r| keep_readable_row("list_due_scheduled_jobs", r))
            .collect();
        Ok(jobs)
    }

    pub fn update_scheduled_job_fired(
        &self,
        id: &Uuid,
        last_fired_at: &DateTime<Utc>,
        next_fire_at: Option<&DateTime<Utc>>,
        enabled: bool,
    ) -> Result<()> {
        let now_str = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let last_str = last_fired_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let next_str = next_fire_at.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        if !enabled {
            let automatic_child_watch: bool = tx.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM scheduled_jobs AS job
                   JOIN agent_spawn_requests AS request
                     ON request.owner_session_id=job.wake_session_id
                    AND job.wake_mode='on_terminal:' || request.child_session_id
                  WHERE job.id=?1 AND request.state='launched'
                 )",
                [id.to_string()],
                |row| row.get(0),
            )?;
            if automatic_child_watch {
                return Err(DaemonError::Store(
                    "automatic child watch requires explicit retirement disposition".into(),
                ));
            }
        }
        tx.execute(
            "UPDATE scheduled_jobs SET last_fired_at = ?1, next_fire_at = COALESCE(?2, next_fire_at),
             enabled = ?3, updated_at = ?4 WHERE id = ?5",
            params![last_str, next_str, enabled as i32, now_str, id.to_string(),],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A continued child starts a new terminal epoch. An enabled ordinary
    /// watch may still carry the prior delivery's confirmation timestamp;
    /// clear it so unrelated owner output cannot retire the next completion.
    pub(crate) fn reset_child_watch_after_continue(&self, id: Uuid) -> Result<bool> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let changed = self.conn.execute(
            "UPDATE scheduled_jobs
             SET last_fired_at=NULL,next_fire_at=?2,updated_at=?2
             WHERE id=?1 AND enabled=1 AND NOT EXISTS (
                 SELECT 1 FROM harness_manager_watches WHERE job_id=?1)",
            params![id.to_string(), now],
        )?;
        Ok(changed == 1)
    }

    /// Stamp only the watch epoch selected before the owner was resumed.
    /// Continuing the child or editing the watch advances `updated_at`, so an
    /// in-flight delivery cannot claim the next completion's witness.
    pub(crate) fn stamp_delivered_child_watch(
        &self,
        id: Uuid,
        observed_updated_at: &DateTime<Utc>,
        delivered_at: &DateTime<Utc>,
        retry_at: &DateTime<Utc>,
    ) -> Result<bool> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let changed = self.conn.execute(
            "UPDATE scheduled_jobs
             SET last_fired_at=?2,next_fire_at=?3,updated_at=?4
             WHERE id=?1 AND enabled=1 AND updated_at=?5
               AND NOT EXISTS (SELECT 1 FROM harness_manager_watches WHERE job_id=?1)",
            params![
                id.to_string(),
                delivered_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                retry_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                now,
                observed_updated_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(changed == 1)
    }

    /// A busy or failed recurring attempt moves the next check. Keep
    /// `updated_at` unchanged while the same watch epoch remains enabled:
    /// an accepted delivery racing this defer must still stamp its witness.
    /// A rearm, edit, retirement, or prior delivery changes the row version
    /// and fences this stale defer instead.
    pub(crate) fn defer_child_watch_attempt(
        &self,
        id: Uuid,
        observed_updated_at: &DateTime<Utc>,
        next_fire_at: Option<&DateTime<Utc>>,
        enabled: bool,
    ) -> Result<bool> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE scheduled_jobs
             SET next_fire_at=COALESCE(?2,next_fire_at),enabled=?3,
                 updated_at=CASE WHEN ?3=0 THEN ?4 ELSE updated_at END
             WHERE id=?1 AND enabled=1 AND updated_at=?5
               AND NOT EXISTS (SELECT 1 FROM harness_manager_watches WHERE job_id=?1)",
            params![
                id.to_string(),
                next_fire_at.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
                i32::from(enabled),
                now,
                observed_updated_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        if changed == 1 && !enabled {
            record_agent_child_watch_transition(&tx, &id, "abandoned", &now)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// A planner can confirm consumption or abandon a watch just before the
    /// child is continued. Retire only the exact row it planned from. `IS`
    /// compares both present timestamps and the never-delivered NULL case.
    pub(crate) fn retire_unchanged_child_watch(
        &self,
        observed: &ScheduledJob,
        disposition: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let retired = Self::retire_unchanged_child_watch_in(&tx, observed, disposition)?;
        tx.commit()?;
        Ok(retired)
    }

    /// Body of [`Self::retire_unchanged_child_watch`] for a caller that
    /// already holds the writer transaction (issue #648 atomic abandonment
    /// retires, records the repair witness, the health fact and the manager
    /// notice in one commit).
    pub(crate) fn retire_unchanged_child_watch_in(
        tx: &Transaction<'_>,
        observed: &ScheduledJob,
        disposition: &str,
    ) -> Result<bool> {
        if !matches!(disposition, "consumed" | "abandoned") {
            return Err(DaemonError::Store(
                "invalid child watch retirement disposition".into(),
            ));
        }
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let changed = tx.execute(
            // The K2 retry state is removed by this same terminal write, so an
            // exhausted watch cannot restart with a fresh budget after a crash.
            "UPDATE scheduled_jobs SET enabled=0,last_fired_at=?2,updated_at=?2,
                 schedule_json=CASE WHEN json_valid(schedule_json)
                     THEN json_remove(schedule_json,'$.continuation_retry')
                     ELSE schedule_json END
             WHERE id=?1 AND enabled=1 AND updated_at=?3
               AND last_fired_at IS ?4
               AND NOT EXISTS (SELECT 1 FROM harness_manager_watches WHERE job_id=?1)",
            params![
                observed.id.to_string(),
                now,
                observed
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                observed
                    .last_fired_at
                    .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
            ],
        )?;
        if changed == 1 {
            record_agent_child_watch_transition(tx, &observed.id, disposition, &now)?;
        }
        Ok(changed == 1)
    }

    /// K2 retry state of a fenced continuation refusal, stored as the
    /// daemon-owned `$.continuation_retry` key of `schedule_json` (no DDL).
    /// `ScheduleSpec` does not model the key, so an operator schedule edit
    /// (`update_scheduled_job` rewrites the spec) resets it, and a
    /// client-supplied key never deserializes into an update.
    pub(crate) fn continuation_retry(&self, id: Uuid) -> Result<Option<ContinuationRetryV1>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT CASE WHEN json_valid(schedule_json)
                        THEN json_extract(schedule_json,'$.continuation_retry') END
                 FROM scheduled_jobs WHERE id=?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        raw.flatten()
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| DaemonError::Store(error.to_string()))
            })
            .transpose()
    }

    /// Record one retryable refusal. Below the bound it moves `next_fire_at`
    /// to `now + min(30s * 2^(attempts-1), 10 min)` without touching
    /// `enabled`, `last_fired_at` or the row version (`updated_at`), so the
    /// wake is retained and a racing delivery can still stamp its epoch.
    /// At 8 attempts or 60 minutes since the first refusal it writes nothing
    /// and returns `Exhausted` for the caller's typed terminal settlement,
    /// which clears the state in its own terminal write. Until that write
    /// commits the recorded attempts stay durable, so a crash in between
    /// re-exhausts on the next refusal instead of granting a fresh budget.
    pub(crate) fn record_continuation_retry(
        &self,
        id: Uuid,
        code: &str,
        tip: Option<Uuid>,
        now: DateTime<Utc>,
        move_next_fire: bool,
    ) -> Result<ContinuationRetryOutcome> {
        let previous = self.continuation_retry(id)?;
        let first_refused_at = previous
            .as_ref()
            .and_then(|retry| DateTime::parse_from_rfc3339(&retry.first_refused_at).ok())
            .map_or(now, |at| at.with_timezone(&Utc));
        let retry = ContinuationRetryV1 {
            attempts: previous.as_ref().map_or(0, |retry| retry.attempts) + 1,
            first_refused_at: first_refused_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            last_code: code.to_string(),
            last_tip: tip,
        };
        if retry.attempts > CONTINUATION_RETRY_MAX_ATTEMPTS
            || now - first_refused_at >= chrono::Duration::minutes(60)
        {
            return Ok(ContinuationRetryOutcome::Exhausted(retry));
        }
        let backoff_seconds = (30_i64 << (retry.attempts - 1).min(5)).min(600);
        let next_fire_at = now + chrono::Duration::seconds(backoff_seconds);
        let json =
            serde_json::to_string(&retry).map_err(|error| DaemonError::Store(error.to_string()))?;
        self.conn.execute(
            "UPDATE scheduled_jobs
             SET schedule_json=json_set(schedule_json,'$.continuation_retry',json(?2)),
                 next_fire_at=CASE WHEN ?4 THEN ?3 ELSE next_fire_at END
             WHERE id=?1 AND json_valid(schedule_json)",
            params![
                id.to_string(),
                json,
                next_fire_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                move_next_fire,
            ],
        )?;
        Ok(ContinuationRetryOutcome::Backoff {
            attempts: retry.attempts,
            next_fire_at,
        })
    }

    /// Clear the retry state on delivery or terminal settlement.
    pub(crate) fn clear_continuation_retry(&self, id: Uuid) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs
             SET schedule_json=json_remove(schedule_json,'$.continuation_retry')
             WHERE id=?1 AND json_valid(schedule_json)
               AND json_extract(schedule_json,'$.continuation_retry') IS NOT NULL",
            params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn update_scheduled_job(&self, id: &Uuid, updates: &ScheduledJobUpdate) -> Result<()> {
        if updates.enabled == Some(true) {
            self.ensure_terminal_watch_enable_within_cap(id)?;
        }

        // Build dynamic SET clause
        let mut sets = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(ref name) = updates.name {
            sets.push(format!("name = ?{idx}"));
            values.push(Box::new(name.clone()));
            idx += 1;
        }
        if let Some(ref message) = updates.message {
            sets.push(format!("message = ?{idx}"));
            values.push(Box::new(message.clone()));
            idx += 1;
        }
        if let Some(ref schedule) = updates.schedule {
            let json =
                serde_json::to_string(schedule).map_err(|e| DaemonError::Store(e.to_string()))?;
            sets.push(format!("schedule_json = ?{idx}"));
            values.push(Box::new(json));
            idx += 1;
        }
        if let Some(enabled) = updates.enabled {
            sets.push(format!("enabled = ?{idx}"));
            values.push(Box::new(enabled as i32));
            idx += 1;
        }
        if let Some(ref next_fire_at) = updates.next_fire_at {
            sets.push(format!("next_fire_at = ?{idx}"));
            values.push(Box::new(
                next_fire_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ));
            idx += 1;
        }

        if sets.is_empty() {
            return Ok(());
        }

        let now_str = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        sets.push(format!("updated_at = ?{idx}"));
        values.push(Box::new(now_str.clone()));
        idx += 1;

        let sql = format!(
            "UPDATE scheduled_jobs SET {} WHERE id = ?{idx}",
            sets.join(", ")
        );
        values.push(Box::new(id.to_string()));

        let params: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(&sql, params.as_slice())?;
        if changed == 1 {
            if let Some(enabled) = updates.enabled {
                record_agent_child_watch_transition(
                    &tx,
                    id,
                    if enabled { "armed" } else { "disabled" },
                    &now_str,
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_scheduled_job(&self, id: &Uuid) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        record_agent_child_watch_transition(&tx, id, "deleted", &now)?;
        tx.execute(
            "DELETE FROM scheduled_jobs WHERE id = ?1",
            params![id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn toggle_scheduled_job(&self, id: &Uuid) -> Result<bool> {
        self.ensure_terminal_watch_enable_within_cap(id)?;
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        tx.execute(
            "UPDATE scheduled_jobs SET enabled = 1 - enabled,
             updated_at = ?1 WHERE id = ?2",
            params![now, id.to_string()],
        )?;
        // Return the new enabled state
        let enabled: i32 = tx.query_row(
            "SELECT enabled FROM scheduled_jobs WHERE id = ?1",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        record_agent_child_watch_transition(
            &tx,
            id,
            if enabled != 0 { "armed" } else { "disabled" },
            &now,
        )?;
        tx.commit()?;
        Ok(enabled != 0)
    }

    /// Enforce the per-recipient terminal-watch ceiling on every existing-row
    /// disabled-to-enabled transition. Operator update/toggle handlers hold the
    /// Store mutex across this check and the following mutation, so concurrent
    /// re-enables cannot both consume the final slot. Already-enabled rows are
    /// idempotent and disabling never enters this guard.
    fn ensure_terminal_watch_enable_within_cap(&self, id: &Uuid) -> Result<()> {
        let row = self
            .conn
            .query_row(
                "SELECT enabled,wake_mode,wake_session_id
                 FROM scheduled_jobs WHERE id=?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((false, Some(wake_mode), Some(wake_session_id))) = row else {
            return Ok(());
        };
        if !wake_mode.starts_with("on_terminal:") {
            return Ok(());
        }
        let enabled: i64 = self.conn.query_row(
            "SELECT count(*) FROM scheduled_jobs
             WHERE enabled=1 AND wake_session_id=?1
               AND wake_mode LIKE 'on_terminal:%'",
            [wake_session_id.as_str()],
            |row| row.get(0),
        )?;
        if enabled >= crate::session::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER as i64 {
            return Err(DaemonError::Store(format!(
                "terminal_watch_cap_reached: session {wake_session_id} already has {enabled} enabled watches (max {})",
                crate::session::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER
            )));
        }
        Ok(())
    }
}

fn wake_mode_string(wake_mode: WakeMode) -> String {
    match wake_mode {
        WakeMode::Fresh => "fresh".to_string(),
        WakeMode::AgentFresh => "agent_fresh".to_string(),
        WakeMode::Resume => "resume".to_string(),
        // A8 terminal watch: data-carrying token. Single writer (here),
        // single reader (`map_scheduled_job_row`), both compile-forced by the
        // enum; the column is daemon-write-once.
        WakeMode::OnTerminal(watched) => format!("on_terminal:{watched}"),
    }
}

/// Decode the durable wake authority carried by a scheduled-job row.
///
/// V120 dependency projections call this same decoder so a row that the
/// scheduler would reject can never be treated as healthy negative evidence.
pub(super) fn decode_scheduled_job_wake_authority(
    wake_mode: Option<&str>,
    wake_session_id: Option<&str>,
) -> Result<(WakeMode, Option<Uuid>)> {
    let wake_mode = match wake_mode.unwrap_or("fresh") {
        "resume" => WakeMode::Resume,
        "agent_fresh" => WakeMode::AgentFresh,
        token => {
            if let Some(raw) = token.strip_prefix("on_terminal:") {
                let watched = Uuid::parse_str(raw).map_err(|error| {
                    DaemonError::Store(format!("invalid on_terminal wake_mode token: {error}"))
                })?;
                WakeMode::OnTerminal(watched)
            } else {
                // Preserve the established compatibility rule for unknown
                // tokens: the scheduler reads them as Fresh.
                WakeMode::Fresh
            }
        }
    };
    let wake_session_id = wake_session_id
        .map(|raw| Uuid::parse_str(raw).map_err(|error| DaemonError::Store(error.to_string())))
        .transpose()?;
    if wake_mode == WakeMode::AgentFresh && wake_session_id.is_none() {
        return Err(DaemonError::Store(
            "agent_fresh wake_mode requires a parseable wake_session_id origin".to_string(),
        ));
    }
    Ok((wake_mode, wake_session_id))
}

pub(super) fn insert_scheduled_job_conn(conn: &Connection, job: &ScheduledJob) -> Result<()> {
    insert_scheduled_job_conn_inner(conn, job, true)
}

/// Manager watches receive their identifying row after the scheduled job is
/// inserted, so the child witness writer cannot identify them from that row.
pub(crate) fn insert_harness_manager_watch_job_conn(
    conn: &Connection,
    job: &ScheduledJob,
) -> Result<()> {
    insert_scheduled_job_conn_inner(conn, job, false)
}

fn insert_scheduled_job_conn_inner(
    conn: &Connection,
    job: &ScheduledJob,
    record_child_witness: bool,
) -> Result<()> {
    let schedule_json =
        serde_json::to_string(&job.schedule).map_err(|e| DaemonError::Store(e.to_string()))?;
    conn.execute(
        "INSERT INTO scheduled_jobs (
            id, name, message, schedule_json, last_fired_at, next_fire_at,
            enabled, working_dir, provider, model, project_id, created_at, updated_at,
            wake_mode, wake_session_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            job.id.to_string(),
            job.name,
            job.message,
            schedule_json,
            job.last_fired_at
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
            job.next_fire_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            job.enabled as i32,
            job.working_dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            job.provider.map(session_provider_to_str),
            job.model,
            job.project_id.map(|u| u.to_string()),
            job.created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            job.updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            wake_mode_string(job.wake_mode),
            job.wake_session_id.map(|u| u.to_string()),
        ],
    )?;
    if record_child_witness {
        record_agent_child_watch_transition(
            conn,
            &job.id,
            "armed",
            &job.updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        )?;
    }
    super::source_worktree_v120::refresh_scheduled_job_path_projection(conn, &job.id.to_string())?;
    Ok(())
}

pub(super) fn get_scheduled_job_conn(conn: &Connection, id: &Uuid) -> Result<Option<ScheduledJob>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                enabled, working_dir, provider, model, project_id, created_at, updated_at,
                wake_mode, wake_session_id
         FROM scheduled_jobs WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_string()], |row| {
        Ok(map_scheduled_job_row(row))
    })?;
    Ok(rows
        .next()
        .and_then(|row| keep_readable_row("get_scheduled_job", row)))
}

/// Insert or reactivate the deterministic terminal recovery wake inside the
/// caller's issue transaction. Returns true only when the exact wake was
/// already enabled with the requested mutable envelope.
pub(super) fn upsert_master_no_idle_recovery_wake_tx(
    tx: &Transaction<'_>,
    wake: &ScheduledJob,
) -> Result<bool> {
    let Some(existing) = get_scheduled_job_conn(tx, &wake.id)? else {
        insert_scheduled_job_conn(tx, wake)?;
        return Ok(false);
    };
    if existing.wake_mode != WakeMode::Resume
        || existing.wake_session_id != wake.wake_session_id
        || !matches!(existing.schedule.recurrence, Recurrence::Once)
    {
        return Err(DaemonError::Store("master_no_idle_wake_id_conflict".into()));
    }
    let exact = existing.enabled
        && existing.project_id == wake.project_id
        && existing.working_dir == wake.working_dir
        && existing.provider == wake.provider
        && existing.model == wake.model;
    if exact {
        return Ok(true);
    }
    tx.execute(
        "UPDATE scheduled_jobs
         SET name = ?1, message = ?2, schedule_json = ?3, enabled = 1,
             next_fire_at = ?4, working_dir = ?5, provider = ?6, model = ?7,
             project_id = ?8, updated_at = ?9
         WHERE id = ?10",
        params![
            wake.name,
            wake.message,
            serde_json::to_string(&wake.schedule)
                .map_err(|error| DaemonError::Store(error.to_string()))?,
            wake.next_fire_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            wake.working_dir
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            wake.provider.map(session_provider_to_str),
            wake.model,
            wake.project_id.map(|id| id.to_string()),
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            wake.id.to_string(),
        ],
    )?;
    super::source_worktree_v120::refresh_scheduled_job_path_projection(tx, &wake.id.to_string())?;
    Ok(exact)
}

/// Insert or re-arm the stable capacity wake while preserving its immutable
/// controller envelope. Each new provider attempt intentionally moves the due
/// slot on this one row.
pub(super) fn upsert_capacity_recovery_wake_tx(
    tx: &Transaction<'_>,
    wake: &ScheduledJob,
) -> Result<()> {
    let Some(existing) = get_scheduled_job_conn(tx, &wake.id)? else {
        insert_scheduled_job_conn(tx, wake)?;
        return Ok(());
    };
    if existing.wake_mode != WakeMode::Resume
        || existing.wake_session_id != wake.wake_session_id
        || !matches!(existing.schedule.recurrence, Recurrence::Once)
        || existing.project_id != wake.project_id
        || existing.working_dir != wake.working_dir
        || existing.provider != wake.provider
        || existing.model != wake.model
        || existing.created_at != wake.created_at
    {
        return Err(DaemonError::Store(
            "capacity_recovery_wake_envelope_conflict".into(),
        ));
    }
    tx.execute(
        "UPDATE scheduled_jobs
         SET name=?1,message=?2,schedule_json=?3,enabled=1,next_fire_at=?4,updated_at=?5
         WHERE id=?6",
        params![
            wake.name,
            wake.message,
            serde_json::to_string(&wake.schedule)
                .map_err(|error| DaemonError::Store(error.to_string()))?,
            wake.next_fire_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            wake.updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            wake.id.to_string(),
        ],
    )?;
    Ok(())
}

/// Flatten one scheduled-job row read, WARN-logging any dropped row. The
/// drop-don't-fail behavior is the documented contract (T-11: a poisoned row —
/// e.g. a malformed `on_terminal:` token — is excluded, never downgraded to
/// `Fresh` and never fails the whole query); A8.1 F-6 adds visibility so a
/// dropped row leaves a daemon-log trace instead of silently vanishing from
/// list / due / get results. Outer `Err` is a rusqlite read error, inner `Err`
/// is a row-mapping error from [`map_scheduled_job_row`].
fn keep_readable_row(
    query: &'static str,
    row: rusqlite::Result<Result<ScheduledJob>>,
) -> Option<ScheduledJob> {
    let err = match row {
        Ok(Ok(job)) => return Some(job),
        Ok(Err(map_err)) => map_err.to_string(),
        Err(read_err) => read_err.to_string(),
    };
    tracing::warn!(query, error = %err, "scheduled_jobs: dropping unreadable row");
    None
}

fn map_scheduled_job_row(row: &rusqlite::Row) -> Result<ScheduledJob> {
    let id_str: String = row.get(0)?;
    let name: String = row.get(1)?;
    let message: String = row.get(2)?;
    let schedule_json: String = row.get(3)?;
    let last_fired_str: Option<String> = row.get(4)?;
    let next_fire_str: String = row.get(5)?;
    let enabled_int: i32 = row.get(6)?;
    let working_dir_str: Option<String> = row.get(7)?;
    let provider_str: Option<String> = row.get(8)?;
    let model: Option<String> = row.get(9)?;
    let project_id_str: Option<String> = row.get(10)?;
    let created_at_str: String = row.get(11)?;
    let updated_at_str: String = row.get(12)?;
    let wake_mode_str: Option<String> = row.get(13).ok().flatten();
    let wake_session_id_str: Option<String> = row.get(14).ok().flatten();

    let schedule: ScheduleSpec = serde_json::from_str(&schedule_json)
        .map_err(|e| DaemonError::Store(format!("invalid schedule_json: {e}")))?;
    let last_fired_at = last_fired_str
        .as_deref()
        .map(parse_timestamp)
        .transpose()
        .map_err(DaemonError::Store)?;
    let next_fire_at = parse_timestamp(&next_fire_str).map_err(DaemonError::Store)?;
    let provider = provider_str
        .map(|s| str_to_session_provider(&s))
        .transpose()?;
    let project_id = project_id_str
        .map(|s| Uuid::parse_str(&s).map_err(|e| DaemonError::Store(e.to_string())))
        .transpose()?;
    let (wake_mode, wake_session_id) = decode_scheduled_job_wake_authority(
        wake_mode_str.as_deref(),
        wake_session_id_str.as_deref(),
    )?;

    Ok(ScheduledJob {
        id: Uuid::parse_str(&id_str).map_err(|e| DaemonError::Store(e.to_string()))?,
        name,
        message,
        schedule,
        last_fired_at,
        next_fire_at,
        enabled: enabled_int != 0,
        working_dir: working_dir_str.map(std::path::PathBuf::from),
        provider,
        model,
        project_id,
        created_at: parse_timestamp(&created_at_str).map_err(DaemonError::Store)?,
        updated_at: parse_timestamp(&updated_at_str).map_err(DaemonError::Store)?,
        wake_mode,
        wake_session_id,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::program_runs::{
        ProgramRunActionKindV1, ProgramRunActionPurposeV1, ProgramRunActionStateV1,
    };
    use rsi_common::types::{Recurrence, ScheduleSpec, WakeMode};

    fn mk_job(wake_mode: WakeMode, wake_session_id: Option<Uuid>) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: "rsi-watch".to_string(),
            message: "arm-time note".to_string(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::EverySeconds(60),
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
            wake_mode,
            wake_session_id,
        }
    }

    #[test]
    fn d05_program_run_wake_job_insert_or_replay_is_exact() {
        let store = Store::open_in_memory().expect("in-memory store");
        let now = Utc::now();
        let action = ProgramRunActionV1 {
            id: Uuid::new_v4(),
            program_run_id: Uuid::new_v4(),
            transition_id: Uuid::new_v4(),
            action_kind: ProgramRunActionKindV1::Wake,
            purpose: ProgramRunActionPurposeV1::RetryWake,
            request_fingerprint: "sha256:wake".into(),
            downstream_dedup_key: "wake:one".into(),
            controller_session_id: Uuid::new_v4(),
            controller_epoch: 1,
            not_before: now,
            state: ProgramRunActionStateV1::Reserved,
            claim_boot_id: None,
            claim_generation: 1,
            claim_run_version: None,
            claim_lease_generation: None,
            claimed_at: None,
            claim_expires_at: None,
            publication_attempts: 0,
            max_publication_attempts: 3,
            external_model_invocation_id: None,
            external_session_id: None,
            scheduled_job_id: None,
            last_error_class: None,
            last_error_message: None,
            created_at: now,
            updated_at: now,
            published_at: None,
            acknowledged_at: None,
        };
        let project_id = Uuid::new_v4();
        let (inserted, replayed) = store
            .insert_or_replay_program_run_wake_job(&action, project_id)
            .expect("first publication");
        assert!(!replayed);
        assert_eq!(inserted.id, action.id);
        assert_eq!(inserted.wake_session_id, Some(action.controller_session_id));

        let (duplicate, replayed) = store
            .insert_or_replay_program_run_wake_job(&action, project_id)
            .expect("exact replay");
        assert!(replayed);
        assert_eq!(duplicate.id, inserted.id);

        store
            .update_scheduled_job_fired(
                &action.id,
                &(now + chrono::Duration::seconds(1)),
                None,
                false,
            )
            .expect("fire wake job");
        let (fired_duplicate, replayed) = store
            .insert_or_replay_program_run_wake_job(&action, project_id)
            .expect("fired wake remains the same immutable envelope");
        assert!(replayed);
        assert_eq!(fired_duplicate.id, action.id);

        let mut changed = action;
        changed.not_before += chrono::Duration::seconds(1);
        let error = store
            .insert_or_replay_program_run_wake_job(&changed, project_id)
            .expect_err("changed replay must fail closed");
        assert!(
            error
                .to_string()
                .contains("program_run_downstream_replay_conflict")
        );
    }

    /// `OnTerminal(uuid)` and `AgentFresh` survive an insert/get round trip;
    /// unknown tokens keep the `_ => Fresh` downgrade contract (§8 rollback
    /// pin); recognized malformed tokens are row errors, not silent Fresh.
    /// (A8.1 F-6: the drop is unchanged but now leaves a `tracing::warn!`
    /// per dropped row — see `keep_readable_row`.)
    #[test]
    fn wake_mode_agent_fresh_roundtrips_and_invalid_origin_fails_closed() {
        let store = Store::open_in_memory().expect("in-memory store");
        let watched = Uuid::new_v4();
        let master = Uuid::new_v4();

        // Round trip.
        let job = mk_job(WakeMode::OnTerminal(watched), Some(master));
        store.insert_scheduled_job(&job).expect("insert");
        let got = store
            .get_scheduled_job(&job.id)
            .expect("get")
            .expect("row present");
        assert_eq!(got.wake_mode, WakeMode::OnTerminal(watched));
        assert_eq!(got.wake_session_id, Some(master));

        // AgentFresh is durable authority provenance and retains its required
        // origin link across a reopen/read boundary.
        let agent_fresh = mk_job(WakeMode::AgentFresh, Some(master));
        store.insert_scheduled_job(&agent_fresh).expect("insert");
        let got = store
            .get_scheduled_job(&agent_fresh.id)
            .expect("get")
            .expect("row present");
        assert_eq!(got.wake_mode, WakeMode::AgentFresh);
        assert_eq!(got.wake_session_id, Some(master));

        // Unknown token -> Fresh (documented downgrade contract).
        let unknown = mk_job(WakeMode::Fresh, None);
        store.insert_scheduled_job(&unknown).expect("insert");
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode = 'some_future_mode' WHERE id = ?1",
                params![unknown.id.to_string()],
            )
            .expect("raw update");
        let got = store
            .get_scheduled_job(&unknown.id)
            .expect("get")
            .expect("row present");
        assert_eq!(got.wake_mode, WakeMode::Fresh);

        // Malformed on_terminal token -> row error (excluded, never Fresh).
        let malformed = mk_job(WakeMode::Fresh, None);
        store.insert_scheduled_job(&malformed).expect("insert");
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode = 'on_terminal:not-a-uuid' WHERE id = ?1",
                params![malformed.id.to_string()],
            )
            .expect("raw update");
        assert!(
            store
                .get_scheduled_job(&malformed.id)
                .expect("get must not error at the call level")
                .is_none(),
            "malformed on_terminal token must surface as a row error, not map to Fresh"
        );
        let listed = store.list_scheduled_jobs().expect("list");
        assert!(
            !listed.iter().any(|j| j.id == malformed.id),
            "malformed row must be excluded from list"
        );
        assert!(listed.iter().any(|j| j.id == job.id));

        // Recognized AgentFresh with a null or malformed origin is unreadable
        // and never downgraded to generic Fresh.
        for origin in [None, Some("not-a-uuid")] {
            let corrupt = mk_job(WakeMode::AgentFresh, Some(master));
            store.insert_scheduled_job(&corrupt).expect("insert");
            store
                .conn
                .execute(
                    "UPDATE scheduled_jobs SET wake_session_id = ?1 WHERE id = ?2",
                    params![origin, corrupt.id.to_string()],
                )
                .expect("raw update");
            assert!(
                store
                    .get_scheduled_job(&corrupt.id)
                    .expect("get must not fail at the call level")
                    .is_none(),
                "invalid agent_fresh origin must be excluded"
            );
        }
    }

    /// The due-jobs query surfaces an armed watch row exactly like any other
    /// enabled job (the scheduler's poll IS the reconcile tick).
    #[test]
    fn on_terminal_job_is_due_listed() {
        let store = Store::open_in_memory().expect("in-memory store");
        let job = mk_job(WakeMode::OnTerminal(Uuid::new_v4()), Some(Uuid::new_v4()));
        store.insert_scheduled_job(&job).expect("insert");
        let due = store
            .list_due_scheduled_jobs(&(Utc::now() + chrono::Duration::seconds(1)))
            .expect("due");
        assert!(due.iter().any(|j| j.id == job.id));
    }
}

/// K2 retry bound: the 8th consecutive retryable refusal exhausts the wake.
pub(crate) const CONTINUATION_RETRY_MAX_ATTEMPTS: u32 = 7;

/// Daemon-owned retry state of a fenced continuation (`$.continuation_retry`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContinuationRetryV1 {
    pub attempts: u32,
    pub first_refused_at: String,
    pub last_code: String,
    pub last_tip: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationRetryOutcome {
    Backoff {
        attempts: u32,
        next_fire_at: DateTime<Utc>,
    },
    Exhausted(ContinuationRetryV1),
}
