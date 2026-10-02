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

/// Deterministic program-guard job ids whose owner session is neither
/// Archived nor Deleted. One sessions scan per retention batch; never per row.
fn live_program_guard_job_ids(conn: &Connection) -> Result<std::collections::HashSet<Uuid>> {
    let mut stmt =
        conn.prepare("SELECT id FROM sessions WHERE status NOT IN ('Archived','Deleted')")?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut guards = std::collections::HashSet::with_capacity(ids.len());
    for raw in ids {
        let session_id = Uuid::parse_str(&raw).map_err(|error| {
            DaemonError::Store(format!(
                "invalid session UUID while resolving program guards: {error}"
            ))
        })?;
        guards.insert(
            crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(
                session_id,
            ),
        );
    }
    Ok(guards)
}

/// `max(last_fired_at, updated_at)`, accepting either 'Z' or '+00:00'
/// RFC3339 offsets. `None` when a present stamp cannot be parsed.
fn retention_last_activity(last_fired_at: Option<&str>, updated_at: &str) -> Option<DateTime<Utc>> {
    let parse = |value: &str| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|parsed| parsed.with_timezone(&Utc))
    };
    let updated = parse(updated_at)?;
    match last_fired_at {
        None => Some(updated),
        Some(fired) => parse(fired).map(|fired| fired.max(updated)),
    }
}

/// Mutable-field update payload for `update_scheduled_job()`.
pub struct ScheduledJobUpdate {
    pub name: Option<String>,
    pub message: Option<String>,
    pub schedule: Option<ScheduleSpec>,
    pub enabled: Option<bool>,
    pub next_fire_at: Option<DateTime<Utc>>,
}

/// Grace period after a job is disabled or last fired before retention may
/// delete it (operator rule, Issue #954).
pub(crate) const RETENTION_GRACE_MINUTES: i64 = 15;

/// Report from one retention batch, or the sum of a run's batches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionSweepReport {
    /// Disabled rows that passed the SQL reference filters and were examined.
    pub scanned: usize,
    /// Examined rows disabled or fired within the grace period.
    pub kept_too_recent: usize,
    /// Examined rows whose timestamps could not be parsed (kept, fail closed).
    pub kept_unreadable: usize,
    /// Program guards whose owner session is not Archived/Deleted.
    pub kept_program_guards: usize,
    /// Deleted scheduled jobs, including superseded manager watches.
    pub deleted: usize,
    /// Of `deleted`, superseded manager watches pruned with their watch row.
    pub deleted_manager_watches: usize,
}

impl RetentionSweepReport {
    pub const fn absorb(&mut self, other: &Self) {
        self.scanned += other.scanned;
        self.kept_too_recent += other.kept_too_recent;
        self.kept_unreadable += other.kept_unreadable;
        self.kept_program_guards += other.kept_program_guards;
        self.deleted += other.deleted;
        self.deleted_manager_watches += other.deleted_manager_watches;
    }
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

    /// Insert an agent-owned job after disabling the owner's other enabled jobs
    /// of the same name, as ONE immediate transaction. Only rows whose
    /// `wake_session_id` is `owner` are touched; daemon-owned program guards and
    /// manager watches are never replaced. Rows are disabled, not deleted.
    /// Returns the ids that were disabled.
    pub(crate) fn insert_scheduled_job_replacing_name(
        &self,
        owner: Uuid,
        job: &ScheduledJob,
    ) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let candidates = self.owned_wake_rows(&tx, owner, None, Some(&job.name), true)?;
        let replaceable: Vec<Uuid> = candidates
            .into_iter()
            .filter(|row| !row.protected)
            .map(|row| row.id)
            .collect();
        disable_scheduled_jobs_conn(&tx, &replaceable)?;
        insert_scheduled_job_conn(&tx, job)?;
        tx.commit()?;
        Ok(replaceable)
    }

    /// Disable the caller's own scheduled job(s) selected by exactly one of
    /// `job_id` or `name`. Ownership is `wake_session_id == owner`; another
    /// session's job is reported as not found. Daemon-owned program guards and
    /// manager watches are refused. Already-disabled owned rows are a no-op
    /// success. Returns the ids that were newly disabled.
    pub(crate) fn cancel_owned_scheduled_jobs(
        &self,
        owner: Uuid,
        job_id: Option<Uuid>,
        name: Option<&str>,
    ) -> Result<Vec<Uuid>> {
        self.cancel_owned_scheduled_jobs_inner(owner, job_id, name, false)
    }

    /// Daemon-side withdrawal of the session's internal background-process
    /// wake. Unlike [`Self::cancel_owned_scheduled_jobs`] (the agent-facing
    /// path) this may disable the reserved `background-process-*` row.
    pub(crate) fn cancel_internal_process_wake(&self, owner: Uuid) -> Result<Vec<Uuid>> {
        let name = format!("{BACKGROUND_PROCESS_WAKE_PREFIX}{owner}");
        self.cancel_owned_scheduled_jobs_inner(owner, None, Some(&name), true)
    }

    fn cancel_owned_scheduled_jobs_inner(
        &self,
        owner: Uuid,
        job_id: Option<Uuid>,
        name: Option<&str>,
        allow_internal: bool,
    ) -> Result<Vec<Uuid>> {
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let rows = self.owned_wake_rows(&tx, owner, job_id, name, false)?;
        if rows.is_empty() {
            return Err(DaemonError::InvalidParam(
                "wake_not_found: no scheduled job of this session matches the selector".into(),
            ));
        }
        let (protected, cancellable): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|row| row.protected || (row.internal && !allow_internal));
        if cancellable.is_empty() && !protected.is_empty() {
            return Err(DaemonError::InvalidParam(
                "wake_protected: daemon-owned program guards, manager watches and background-process wakes cannot be cancelled"
                    .into(),
            ));
        }
        let to_disable: Vec<Uuid> = cancellable
            .into_iter()
            .filter(|row| row.enabled)
            .map(|row| row.id)
            .collect();
        disable_scheduled_jobs_conn(&tx, &to_disable)?;
        tx.commit()?;
        Ok(to_disable)
    }

    /// Bounded read of the scheduled jobs owned by `owner`
    /// (`wake_session_id == owner`), soonest fire first. Disabled history is
    /// included only when asked for. Returns `(job, protected)` where
    /// `protected` marks daemon-owned program guards and manager watches.
    pub(crate) fn list_owned_scheduled_jobs(
        &self,
        owner: Uuid,
        include_disabled: bool,
        limit: usize,
    ) -> Result<Vec<(ScheduledJob, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                    enabled, working_dir, provider, model, project_id, created_at, updated_at,
                    wake_mode, wake_session_id
             FROM scheduled_jobs
             WHERE wake_session_id = ?1 AND (?2 = 1 OR enabled = 1)
             ORDER BY enabled DESC, next_fire_at ASC, id ASC
             LIMIT ?3",
        )?;
        let jobs: Vec<ScheduledJob> = stmt
            .query_map(
                params![owner.to_string(), include_disabled as i32, limit as i64],
                |row| Ok(map_scheduled_job_row(row)),
            )?
            .filter_map(|r| keep_readable_row("list_owned_scheduled_jobs", r))
            .collect();
        let mut out = Vec::with_capacity(jobs.len());
        for job in jobs {
            let protected = is_background_process_wake_name(&job.name)
                || self.program_guard_owner_for_job_id(&job.id)?.is_some()
                || self.is_harness_manager_watch(job.id)?;
            out.push((job, protected));
        }
        Ok(out)
    }

    /// Daemon-settings key recording the resume wakes a `pause_lead` suspended
    /// for `lead`, so `resume_lead` restores exactly those.
    pub(crate) fn suspended_lead_wakes_key(lead: Uuid) -> String {
        format!("manager_suspended_wakes:{lead}")
    }

    fn read_suspended_lead_wakes(&self, lead: Uuid) -> Result<Vec<Uuid>> {
        let Some(raw) = self.get_daemon_setting(&Self::suspended_lead_wakes_key(lead))? else {
            return Ok(Vec::new());
        };
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| DaemonError::Store(format!("invalid suspended wake record: {e}")))?;
        Ok(value["job_ids"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().and_then(|id| Uuid::parse_str(id).ok()))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// #1042: disable `lead`'s own enabled `resume` wakes for a manager
    /// `pause_lead` and record exactly which jobs were suspended (a
    /// `daemon_settings` row, so no schema change). Runs on the caller's
    /// connection, hence inside the caller's transaction. Program guards and
    /// manager watches are never suspended; rows are disabled, never deleted.
    /// Earlier still-unrestored suspensions are kept, so a repeated pause
    /// cannot lose the record. Returns the ids newly suspended.
    pub(crate) fn suspend_lead_resume_wakes(
        &self,
        lead: Uuid,
        action_id: Uuid,
    ) -> Result<Vec<Uuid>> {
        let mut candidates = Vec::new();
        for row in self.owned_wake_rows_by_mode(lead, "resume")? {
            if !row.protected {
                candidates.push(row.id);
            }
        }
        if candidates.is_empty() {
            return Ok(candidates);
        }
        disable_scheduled_jobs_conn(&self.conn, &candidates)?;
        let mut all = self.read_suspended_lead_wakes(lead)?;
        for id in &candidates {
            if !all.contains(id) {
                all.push(*id);
            }
        }
        let record = serde_json::json!({
            "action_id": action_id,
            "job_ids": all,
        });
        self.set_daemon_setting(&Self::suspended_lead_wakes_key(lead), &record.to_string())?;
        Ok(candidates)
    }

    /// #1042: re-enable exactly the jobs a `pause_lead` suspended for `lead`
    /// (still disabled resume jobs of that lead), then clear the record.
    /// Jobs that were meanwhile replaced or re-enabled are left as they are.
    /// Returns the ids restored.
    pub(crate) fn restore_lead_suspended_wakes(&self, lead: Uuid) -> Result<Vec<Uuid>> {
        let recorded = self.read_suspended_lead_wakes(lead)?;
        if recorded.is_empty() {
            return Ok(recorded);
        }
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut restored = Vec::new();
        for id in recorded {
            let changed = self.conn.execute(
                "UPDATE scheduled_jobs SET enabled = 1, updated_at = ?2
                 WHERE id = ?1 AND enabled = 0 AND wake_mode = 'resume' AND wake_session_id = ?3",
                params![id.to_string(), now, lead.to_string()],
            )?;
            if changed == 1 {
                restored.push(id);
            }
        }
        self.set_daemon_setting(
            &Self::suspended_lead_wakes_key(lead),
            &serde_json::json!({ "job_ids": Vec::<Uuid>::new() }).to_string(),
        )?;
        Ok(restored)
    }

    /// Enabled jobs of `owner` with the given stored `wake_mode`.
    fn owned_wake_rows_by_mode(&self, owner: Uuid, mode: &str) -> Result<Vec<OwnedWakeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM scheduled_jobs
             WHERE wake_session_id = ?1 AND wake_mode = ?2 AND enabled = 1",
        )?;
        let ids = stmt
            .query_map(params![owner.to_string(), mode], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            let id = Uuid::parse_str(&id)
                .map_err(|e| DaemonError::Store(format!("invalid scheduled job id: {e}")))?;
            let protected = self.program_guard_owner_for_job_id(&id)?.is_some()
                || self.is_harness_manager_watch(id)?;
            rows.push(OwnedWakeRow {
                id,
                enabled: true,
                protected,
                internal: false,
            });
        }
        Ok(rows)
    }

    fn owned_wake_rows(
        &self,
        tx: &Connection,
        owner: Uuid,
        job_id: Option<Uuid>,
        name: Option<&str>,
        enabled_only: bool,
    ) -> Result<Vec<OwnedWakeRow>> {
        let mut stmt = tx.prepare(
            "SELECT id, enabled, name FROM scheduled_jobs
             WHERE wake_session_id = ?1
               AND (?2 IS NULL OR id = ?2)
               AND (?3 IS NULL OR name = ?3)
               AND (?4 = 0 OR enabled = 1)
               AND (?2 IS NOT NULL OR ?3 IS NOT NULL)",
        )?;
        let raw = stmt
            .query_map(
                params![
                    owner.to_string(),
                    job_id.map(|id| id.to_string()),
                    name,
                    enabled_only as i32
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(raw.len());
        for (id, enabled, job_name) in raw {
            let id = Uuid::parse_str(&id)
                .map_err(|e| DaemonError::Store(format!("invalid scheduled job id: {e}")))?;
            let protected = self.program_guard_owner_for_job_id(&id)?.is_some()
                || self.is_harness_manager_watch(id)?;
            rows.push(OwnedWakeRow {
                id,
                enabled,
                protected,
                internal: is_background_process_wake_name(&job_name),
            });
        }
        Ok(rows)
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

    /// One page of scheduled jobs for the operator RPC (Issue #954 B), newest
    /// created first. Ordering is the immutable key `(created_at, id)`, so a
    /// cursor built from the last row stays valid however rows are added,
    /// fired, disabled or deleted between pages. The default filter keeps
    /// enabled rows plus rows fired or updated (disabled) within
    /// [`RETENTION_GRACE_MINUTES`] of `now`, and rows whose `updated_at` cannot
    /// be parsed (fail open, matching retention's fail-closed keep);
    /// `include_history` returns everything. Returns the page and, when more
    /// rows follow, the cursor for the next page.
    pub fn list_scheduled_jobs_page(
        &self,
        now: DateTime<Utc>,
        include_history: bool,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<ScheduledJob>, Option<String>)> {
        let limit = limit.max(1);
        let after = cursor.map(parse_scheduled_jobs_cursor).transpose()?;
        let cutoff = (now - chrono::Duration::minutes(RETENTION_GRACE_MINUTES))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut stmt = self.conn.prepare(
            "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                    enabled, working_dir, provider, model, project_id, created_at, updated_at,
                    wake_mode, wake_session_id
             FROM scheduled_jobs
             WHERE (?1 = 1
                    OR enabled = 1
                    OR julianday(updated_at) IS NULL
                    OR julianday(updated_at) >= julianday(?2)
                    OR julianday(last_fired_at) >= julianday(?2))
               AND (?3 IS NULL OR created_at < ?3 OR (created_at = ?3 AND id < ?4))
             ORDER BY created_at DESC, id DESC
             LIMIT ?5",
        )?;
        let (after_created, after_id) = match &after {
            Some((created, id)) => (Some(created.as_str()), Some(id.as_str())),
            None => (None, None),
        };
        let fetch = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let raw = stmt
            .query_map(
                params![
                    include_history as i64,
                    cutoff,
                    after_created,
                    after_id,
                    fetch
                ],
                |row| {
                    let created_at: String = row.get(11)?;
                    let id: String = row.get(0)?;
                    Ok((created_at, id, map_scheduled_job_row(row)))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = raw.len() > limit;
        let mut next_cursor = None;
        let mut jobs = Vec::with_capacity(raw.len().min(limit));
        for (created_at, id, mapped) in raw.into_iter().take(limit) {
            next_cursor = Some(format!("{id}@{created_at}"));
            if let Some(job) = keep_readable_row("list_scheduled_jobs_page", Ok(mapped)) {
                jobs.push(job);
            }
        }
        Ok((jobs, if has_more { next_cursor } else { None }))
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

    /// Read the raw enablement bit even when another column makes the row
    /// undecodable. Terminal repair must never re-arm a closed sentinel.
    pub(crate) fn enabled_scheduled_job_exists(&self, id: &Uuid) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scheduled_jobs WHERE id = ?1 AND enabled = 1)",
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
        let backoff_seconds = continuation_backoff_seconds(retry.attempts);
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

    /// One bounded retention batch (Issue #954 operator rule).
    ///
    /// Deletes a DISABLED job once `max(last_fired_at, updated_at)` is at least
    /// [`RETENTION_GRACE_MINUTES`] old, except:
    /// - program guards whose owner session is not Archived/Deleted;
    /// - rows referenced by a RESTRICT foreign key;
    /// - jobs referenced by a Starting/Running/WaitingApproval session;
    /// - manager watches of the CURRENT scope version, or with an open notice.
    ///
    /// A manager watch job id is a `UUIDv5` over a key that embeds the scope
    /// `row_version`, and reconcile derives ids only from the current scope, so
    /// a superseded watch can never be re-created or re-armed; it is pruned
    /// together with its `harness_manager_watches` row.
    ///
    /// Reference exclusions are applied in SQL so permanently kept rows never
    /// occupy a page; the keyset cursor (`after`, ascending id) always advances.
    /// Selection and deletion share one IMMEDIATE transaction per batch, and
    /// every read error aborts the batch without deleting (fail closed).
    /// Returns the next cursor, or `None` once the table is exhausted.
    ///
    /// # Errors
    /// Returns an error, having deleted nothing, when any read or write in the
    /// batch transaction fails.
    pub fn retention_sweep_batch(
        &self,
        now: DateTime<Utc>,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<(RetentionSweepReport, Option<Uuid>)> {
        let mut report = RetentionSweepReport::default();
        let cutoff = now - chrono::Duration::minutes(RETENTION_GRACE_MINUTES);
        let limit = limit.max(1);
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;

        let live_guards = live_program_guard_job_ids(&tx)?;
        let page: Vec<(String, Option<String>, String, bool)> = {
            let mut stmt = tx.prepare(
                "SELECT j.id, j.last_fired_at, j.updated_at,
                        EXISTS(SELECT 1 FROM harness_manager_watches w WHERE w.job_id=j.id)
                   FROM scheduled_jobs j
                  WHERE j.enabled=0
                    AND CASE WHEN json_valid(j.schedule_json)
                             THEN COALESCE(json_type(j.schedule_json,'$.transient_heal')='object',0)
                             ELSE 0 END=0
                    AND (?1 IS NULL OR j.id > ?1)
                    AND NOT EXISTS (SELECT 1 FROM sandbox_custody_events r
                                     WHERE r.scheduled_job_id=j.id)
                    AND NOT EXISTS (SELECT 1 FROM idea_program_run_actions r
                                     WHERE r.scheduled_job_id=j.id)
                    AND NOT EXISTS (SELECT 1 FROM master_no_idle_capacity_incidents r
                                     WHERE r.wake_job_id=j.id OR r.program_guard_job_id=j.id)
                    AND NOT EXISTS (SELECT 1 FROM master_no_idle_capacity_attempts r
                                     WHERE r.delivery_wake_job_id=j.id)
                    AND NOT EXISTS (SELECT 1 FROM sessions s
                                     WHERE s.scheduled_job_id=j.id
                                       AND s.status IN ('Starting','Running','WaitingApproval'))
                    AND NOT EXISTS (
                        SELECT 1 FROM harness_manager_watches w
                         WHERE w.job_id=j.id
                           AND (w.scope_version IS (SELECT c.row_version
                                                      FROM harness_manager_scopes c
                                                     WHERE c.project_id=w.project_id)
                                OR EXISTS (SELECT 1 FROM harness_manager_notices n
                                            WHERE n.job_id=j.id
                                              AND n.retired_at IS NULL
                                              AND n.settled_at IS NULL)))
                  ORDER BY j.id
                  LIMIT ?2",
            )?;
            stmt.query_map(
                params![
                    after.map(|id| id.to_string()),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?
            .collect::<rusqlite::Result<_>>()?
        };
        let next = if page.len() < limit {
            None
        } else {
            let last = &page[page.len() - 1].0;
            Some(Uuid::parse_str(last).map_err(|error| {
                DaemonError::Store(format!(
                    "invalid scheduled job id in retention page: {error}"
                ))
            })?)
        };
        report.scanned = page.len();

        let now_str = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for (raw_id, last_fired_at, updated_at, is_manager_watch) in page {
            let Ok(id) = Uuid::parse_str(&raw_id) else {
                report.kept_unreadable += 1;
                continue;
            };
            let Some(last_activity) =
                retention_last_activity(last_fired_at.as_deref(), &updated_at)
            else {
                report.kept_unreadable += 1;
                continue;
            };
            if last_activity > cutoff {
                report.kept_too_recent += 1;
                continue;
            }
            if live_guards.contains(&id) {
                report.kept_program_guards += 1;
                continue;
            }
            // Record the witness while any manager-watch row still exists so
            // the transition matches `delete_scheduled_job` bookkeeping.
            record_agent_child_watch_transition(&tx, &id, "deleted", &now_str)?;
            if is_manager_watch {
                tx.execute(
                    "DELETE FROM harness_manager_watches WHERE job_id=?1",
                    params![id.to_string()],
                )?;
                report.deleted_manager_watches += 1;
            }
            let deleted = tx.execute(
                "DELETE FROM scheduled_jobs WHERE id=?1 AND enabled=0",
                params![id.to_string()],
            )?;
            report.deleted += deleted;
        }
        tx.commit()?;
        Ok((report, next))
    }

    /// Run retention batches until the table is exhausted or `max_batches`
    /// batches ran. The daemon scheduler drives batches itself so it can
    /// release the store lock between them; this helper serves tests and
    /// operator tooling.
    ///
    /// # Errors
    /// Returns the first failing batch's error; earlier batches stay committed.
    pub fn retention_sweep(
        &self,
        now: DateTime<Utc>,
        batch_limit: usize,
        max_batches: usize,
    ) -> Result<RetentionSweepReport> {
        let mut total = RetentionSweepReport::default();
        let mut cursor = None;
        for _ in 0..max_batches.max(1) {
            let (report, next) = self.retention_sweep_batch(now, cursor, batch_limit)?;
            total.absorb(&report);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(total)
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

/// Reserved name prefix of the daemon-internal Harness background-process
/// idle-completion wake; agents can neither create nor cancel such a job.
pub(crate) const BACKGROUND_PROCESS_WAKE_PREFIX: &str = "background-process-";

fn is_background_process_wake_name(name: &str) -> bool {
    name.starts_with(BACKGROUND_PROCESS_WAKE_PREFIX)
}

struct OwnedWakeRow {
    id: Uuid,
    enabled: bool,
    protected: bool,
    /// Daemon-internal Harness background-process wake (reserved name
    /// prefix). Replaceable by the registry but never agent-cancellable.
    internal: bool,
}

/// Flip `enabled` to 0 for each id through the same witness bookkeeping as
/// [`Store::update_scheduled_job`]; rows are never deleted.
fn disable_scheduled_jobs_conn(conn: &Connection, ids: &[Uuid]) -> Result<()> {
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    for id in ids {
        let changed = conn.execute(
            "UPDATE scheduled_jobs SET enabled = 0, updated_at = ?2 WHERE id = ?1 AND enabled = 1",
            params![id.to_string(), now],
        )?;
        if changed == 1 {
            record_agent_child_watch_transition(conn, id, "disabled", &now)?;
        }
    }
    Ok(())
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
/// Splits a `ListScheduledJobs` cursor (`<id>@<created_at>`) into
/// `(created_at, id)` as stored. The id must be a lowercase UUID so a forged
/// cursor cannot smuggle an arbitrary comparison key.
fn parse_scheduled_jobs_cursor(cursor: &str) -> Result<(String, String)> {
    let invalid = || DaemonError::InvalidParam("invalid ListScheduledJobs cursor".into());
    let (id, created_at) = cursor.split_once('@').ok_or_else(invalid)?;
    let parsed = Uuid::parse_str(id).map_err(|_| invalid())?;
    if parsed.to_string() != id || created_at.is_empty() {
        return Err(invalid());
    }
    Ok((created_at.to_string(), id.to_string()))
}

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

/// Daemon-evaluated wait predicates (#1006): the predicate and its bookkeeping
/// are the daemon-owned `$.wake_when` key of `schedule_json` (no DDL). Like
/// `$.continuation_retry`, `ScheduleSpec` does not model the key, so an
/// operator schedule edit drops it and a client-supplied key never
/// deserializes into an update.
impl Store {
    /// Arm a predicate wake in one immediate transaction: every job id the
    /// predicate names must exist and belong to the wake's owner
    /// (`wake_when_job_not_found` otherwise, so a foreign id is
    /// indistinguishable from an unknown one), the owner's enabled predicate
    /// wakes stay under the cap, an explicit name replaces the owner's earlier
    /// enabled unprotected job of that name, and the row is inserted with its
    /// `$.wake_when` state. Returns the ids the name replacement disabled.
    pub(crate) fn insert_wake_when(
        &self,
        job: &ScheduledJob,
        state: &rsi_common::wake_predicate::WakeWhenState,
        replace_name: bool,
    ) -> Result<Vec<Uuid>> {
        use rsi_common::wake_predicate as wp;
        let owner = job.wake_session_id.ok_or_else(|| {
            DaemonError::InvalidParam("mode 'when' requires a known origin session id".into())
        })?;
        let tx = Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        if let Some(ids) = &state.predicate.jobs_terminal {
            for id in ids {
                let owned: Option<String> = tx
                    .query_row(
                        "SELECT owner_session_id FROM agent_jobs WHERE id=?1",
                        params![id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if owned.as_deref() != Some(owner.to_string().as_str()) {
                    return Err(DaemonError::InvalidParam(
                        wp::WAKE_WHEN_JOB_NOT_FOUND.into(),
                    ));
                }
            }
        }
        let replaceable: Vec<Uuid> = if replace_name {
            self.owned_wake_rows(&tx, owner, None, Some(&job.name), true)?
                .into_iter()
                .filter(|row| !row.protected)
                .map(|row| row.id)
                .collect()
        } else {
            Vec::new()
        };
        disable_scheduled_jobs_conn(&tx, &replaceable)?;
        let held: i64 = tx.query_row(
            "SELECT COUNT(*) FROM scheduled_jobs
             WHERE wake_session_id=?1 AND enabled=1 AND json_valid(schedule_json)
               AND json_extract(schedule_json,'$.wake_when') IS NOT NULL",
            params![owner.to_string()],
            |row| row.get(0),
        )?;
        if held >= wp::WAKE_WHEN_MAX_PER_SESSION as i64 {
            return Err(DaemonError::InvalidParam(wp::WAKE_WHEN_CAP_REACHED.into()));
        }
        insert_scheduled_job_conn(&tx, job)?;
        let json =
            serde_json::to_string(state).map_err(|error| DaemonError::Store(error.to_string()))?;
        tx.execute(
            "UPDATE scheduled_jobs
             SET schedule_json=json_set(schedule_json,'$.wake_when',json(?2))
             WHERE id=?1 AND json_valid(schedule_json)",
            params![job.id.to_string(), json],
        )?;
        tx.commit()?;
        Ok(replaceable)
    }

    /// The predicate state of a wake row; `None` for every ordinary row.
    pub(crate) fn wake_when_state(
        &self,
        id: Uuid,
    ) -> Result<Option<rsi_common::wake_predicate::WakeWhenState>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT CASE WHEN json_valid(schedule_json)
                        THEN json_extract(schedule_json,'$.wake_when') END
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

    /// Test fixture: replace a row's predicate state (for example to expire
    /// its deadline without waiting).
    #[cfg(test)]
    pub(crate) fn set_wake_when_state_for_test(
        &self,
        id: Uuid,
        state: &rsi_common::wake_predicate::WakeWhenState,
    ) -> Result<()> {
        let json =
            serde_json::to_string(state).map_err(|error| DaemonError::Store(error.to_string()))?;
        self.conn.execute(
            "UPDATE scheduled_jobs
             SET schedule_json=json_set(schedule_json,'$.wake_when',json(?2)) WHERE id=?1",
            params![id.to_string(), json],
        )?;
        Ok(())
    }

    /// Enabled predicate wakes due at `now`: the scheduler's fast lane, which
    /// evaluates them far more often than the general due poll.
    pub(crate) fn list_due_wake_when_jobs(&self, now: &DateTime<Utc>) -> Result<Vec<ScheduledJob>> {
        let now_str = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut stmt = self.conn.prepare(
            "SELECT id, name, message, schedule_json, last_fired_at, next_fire_at,
                    enabled, working_dir, provider, model, project_id, created_at, updated_at,
                    wake_mode, wake_session_id
             FROM scheduled_jobs
             WHERE enabled = 1 AND next_fire_at <= ?1 AND json_valid(schedule_json)
               AND json_extract(schedule_json,'$.wake_when') IS NOT NULL
             ORDER BY next_fire_at ASC",
        )?;
        let jobs = stmt
            .query_map(params![now_str], |row| Ok(map_scheduled_job_row(row)))?
            .filter_map(|r| keep_readable_row("list_due_wake_when_jobs", r))
            .collect();
        Ok(jobs)
    }

    /// A pending predicate stays armed: move only `next_fire_at`, and only of
    /// a still-enabled row, leaving `last_fired_at` and `updated_at` alone.
    pub(crate) fn defer_wake_when(&self, id: Uuid, next_fire_at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs SET next_fire_at=?2 WHERE id=?1 AND enabled=1",
            params![
                id.to_string(),
                next_fire_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::program_runs::{
        ProgramRunActionKindV1, ProgramRunActionPurposeV1, ProgramRunActionStateV1,
    };
    use rsi_common::types::{Recurrence, ScheduleSpec, SessionStatus, WakeMode};

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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
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
    // --- Issue #954 retention sweep: positive end-state assertions ---

    fn retention_job(
        id: Uuid,
        enabled: bool,
        age: chrono::Duration,
        wake_session_id: Option<Uuid>,
    ) -> ScheduledJob {
        let at = Utc::now() - age;
        ScheduledJob {
            id,
            name: format!("retention-{id}"),
            message: String::new(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: at,
            },
            last_fired_at: Some(at),
            next_fire_at: at,
            enabled,
            working_dir: Some(std::path::PathBuf::from("/var/tmp/retention")),
            provider: None,
            model: None,
            project_id: None,
            created_at: at,
            updated_at: at,
            wake_mode: WakeMode::Resume,
            wake_session_id,
        }
    }

    fn old() -> chrono::Duration {
        chrono::Duration::minutes(super::RETENTION_GRACE_MINUTES + 15)
    }

    fn job_exists(store: &Store, id: Uuid) -> bool {
        store.scheduled_job_exists(&id).expect("exists")
    }

    fn enable_foreign_keys(store: &Store) {
        store
            .conn
            .execute_batch("PRAGMA foreign_keys=ON;")
            .expect("foreign keys on");
        let on: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(on, 1, "sweep must run with RESTRICT enforced");
    }

    /// Insert a minimal row into `table` whose `column` references `job_id`.
    fn insert_reference(store: &Store, table: &str, column: &str, job_id: Uuid) {
        insert_row(store, table, &[(column, job_id.to_string().into())]);
    }

    /// Insert a minimal row into `table` with the given column values; every
    /// other NOT NULL column without a default gets a unique dummy value.
    /// Test fixture only: CHECK, FK and validation-trigger enforcement are off
    /// while the row is written, then foreign keys are re-enabled.
    fn insert_row(store: &Store, table: &str, fixed: &[(&str, rusqlite::types::Value)]) {
        store
            .conn
            .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;")
            .expect("fixture pragmas");
        let columns: Vec<(String, String, bool, bool)> = store
            .conn
            .prepare(&format!("SELECT name,type,\"notnull\",dflt_value IS NOT NULL FROM pragma_table_info('{table}')"))
            .expect("table info")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .expect("columns")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        let mut names = Vec::new();
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        for (name, kind, not_null, has_default) in columns {
            if let Some((_, value)) = fixed.iter().find(|(column, _)| *column == name) {
                names.push(name);
                values.push(value.clone());
            } else if not_null && !has_default {
                values.push(if kind.to_ascii_uppercase().contains("INT") {
                    rusqlite::types::Value::Integer(1)
                } else {
                    Uuid::new_v4().to_string().into()
                });
                names.push(name);
            }
        }
        // Validation triggers (e.g. V79 ProgramRun identity) are fixture noise
        // here: lift them for this one insert and restore their exact SQL.
        let triggers: Vec<(String, String)> = store
            .conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type='trigger' AND tbl_name=?1")
            .expect("triggers")
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("trigger rows")
            .collect::<rusqlite::Result<_>>()
            .expect("collect triggers");
        for (name, _) in &triggers {
            store
                .conn
                .execute_batch(&format!("DROP TRIGGER \"{name}\";"))
                .expect("lift trigger");
        }
        let placeholders = (1..=names.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO {table}({}) VALUES({placeholders})",
                    names.join(",")
                ),
                rusqlite::params_from_iter(values),
            )
            .expect("insert reference");
        for (_, sql) in &triggers {
            store.conn.execute_batch(sql).expect("restore trigger");
        }
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints=OFF;")
            .expect("checks back on");
        enable_foreign_keys(store);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_deletes_old_disabled_with_projection_and_keeps_enabled_and_recent() {
        let store = Store::open_in_memory().expect("store");
        enable_foreign_keys(&store);
        let (eligible, enabled, recent) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        store
            .insert_scheduled_job(&retention_job(eligible, false, old(), None))
            .unwrap();
        store
            .insert_scheduled_job(&retention_job(enabled, true, old(), None))
            .unwrap();
        store
            .insert_scheduled_job(&retention_job(
                recent,
                false,
                chrono::Duration::minutes(5),
                None,
            ))
            .unwrap();
        let projections = |id: Uuid| -> i64 {
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM scheduled_job_path_projections WHERE job_id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(projections(eligible), 1);

        let report = store.retention_sweep(Utc::now(), 200, 50).expect("sweep");

        assert_eq!(report.deleted, 1);
        assert_eq!(report.kept_too_recent, 1);
        assert!(!job_exists(&store, eligible));
        assert_eq!(projections(eligible), 0, "projection cascades with the job");
        assert!(job_exists(&store, enabled), "enabled job kept");
        assert!(
            job_exists(&store, recent),
            "job inside the grace period kept"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_ages_offset_timestamps_and_keeps_unreadable_rows() {
        let store = Store::open_in_memory().expect("store");
        let (offset_old, offset_recent) = (Uuid::new_v4(), Uuid::new_v4());
        store
            .insert_scheduled_job(&retention_job(offset_old, false, old(), None))
            .unwrap();
        store
            .insert_scheduled_job(&retention_job(
                offset_recent,
                false,
                chrono::Duration::minutes(1),
                None,
            ))
            .unwrap();
        for id in [offset_old, offset_recent] {
            // Rewrite both stamps in '+00:00' form, as older writers produced.
            store
                .conn
                .execute(
                    "UPDATE scheduled_jobs
                        SET updated_at=replace(updated_at,'Z','+00:00'),
                            last_fired_at=replace(last_fired_at,'Z','+00:00')
                      WHERE id=?1",
                    [id.to_string()],
                )
                .unwrap();
        }

        let report = store.retention_sweep(Utc::now(), 200, 50).expect("sweep");

        assert_eq!(report.deleted, 1);
        assert!(!job_exists(&store, offset_old));
        assert!(job_exists(&store, offset_recent));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_keeps_live_owner_program_guard_and_deletes_archived_owner_guard() {
        let store = Store::open_in_memory().expect("store");
        let live_owner = Uuid::new_v4();
        let archived_owner = Uuid::new_v4();
        for (owner, status) in [
            (live_owner, SessionStatus::Completed),
            (archived_owner, SessionStatus::Archived),
        ] {
            let mut session = crate::session::agent_verbs::tests::test_session(
                owner,
                std::path::PathBuf::from("/var/tmp/retention"),
            );
            session.status = status;
            store.insert_session(&session).expect("session");
        }
        let guard = |owner| {
            crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(owner)
        };
        for owner in [live_owner, archived_owner] {
            store
                .insert_scheduled_job(&retention_job(guard(owner), false, old(), Some(owner)))
                .unwrap();
        }

        let report = store.retention_sweep(Utc::now(), 200, 50).expect("sweep");

        assert_eq!(report.kept_program_guards, 1);
        assert_eq!(report.deleted, 1);
        assert!(
            job_exists(&store, guard(live_owner)),
            "closed guard of a resumable owner stays"
        );
        assert!(!job_exists(&store, guard(archived_owner)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_keeps_restrict_referenced_and_active_session_jobs() {
        let store = Store::open_in_memory().expect("store");
        let restrict_references = [
            ("sandbox_custody_events", "scheduled_job_id"),
            ("idea_program_run_actions", "scheduled_job_id"),
            ("master_no_idle_capacity_incidents", "wake_job_id"),
            ("master_no_idle_capacity_incidents", "program_guard_job_id"),
            ("master_no_idle_capacity_attempts", "delivery_wake_job_id"),
        ];
        let mut referenced_jobs = Vec::new();
        for (table, column) in restrict_references {
            let id = Uuid::new_v4();
            store
                .insert_scheduled_job(&retention_job(id, false, old(), None))
                .unwrap();
            insert_reference(&store, table, column, id);
            referenced_jobs.push(id);
        }
        let active_job = Uuid::new_v4();
        let finished_job = Uuid::new_v4();
        for (job, status) in [
            (active_job, SessionStatus::Running),
            (finished_job, SessionStatus::Completed),
        ] {
            store
                .insert_scheduled_job(&retention_job(job, false, old(), None))
                .unwrap();
            let session_id = Uuid::new_v4();
            let mut session = crate::session::agent_verbs::tests::test_session(
                session_id,
                std::path::PathBuf::from("/var/tmp/retention"),
            );
            session.status = status;
            store.insert_session(&session).expect("session");
            store
                .conn
                .execute(
                    "UPDATE sessions SET scheduled_job_id=?1 WHERE id=?2",
                    [job.to_string(), session_id.to_string()],
                )
                .unwrap();
        }
        let plain = Uuid::new_v4();
        store
            .insert_scheduled_job(&retention_job(plain, false, old(), None))
            .unwrap();
        enable_foreign_keys(&store);

        let report = store.retention_sweep(Utc::now(), 200, 50).expect("sweep");

        for id in &referenced_jobs {
            assert!(job_exists(&store, *id), "RESTRICT-referenced job kept");
        }
        assert!(
            job_exists(&store, active_job),
            "active session reference kept"
        );
        assert!(!job_exists(&store, finished_job));
        assert!(!job_exists(&store, plain));
        assert_eq!(report.deleted, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_pages_past_permanently_kept_rows() {
        let store = Store::open_in_memory().expect("store");
        enable_foreign_keys(&store);
        let now = Utc::now();
        // 300 kept rows whose ids sort before every eligible row.
        let mut kept = Vec::new();
        for index in 0..300u128 {
            let id = Uuid::from_u128(index + 1);
            store
                .insert_scheduled_job(&retention_job(id, false, old(), None))
                .unwrap();
            insert_reference(&store, "sandbox_custody_events", "scheduled_job_id", id);
            kept.push(id);
        }
        // 300 closed guards of resumable owners, also sorting first.
        for index in 0..300u128 {
            let owner = Uuid::from_u128(10_000 + index);
            let session = crate::session::agent_verbs::tests::test_session(
                owner,
                std::path::PathBuf::from("/var/tmp/retention"),
            );
            store.insert_session(&session).expect("session");
            let guard =
                crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(
                    owner,
                );
            store
                .insert_scheduled_job(&retention_job(guard, false, old(), Some(owner)))
                .unwrap();
            kept.push(guard);
        }
        let eligible: Vec<Uuid> = (0..5u128)
            .map(|index| Uuid::from_u128(u128::MAX - index))
            .collect();
        for id in &eligible {
            store
                .insert_scheduled_job(&retention_job(*id, false, old(), None))
                .unwrap();
        }

        // Drive batches exactly as the scheduler does.
        let mut cursor = None;
        let mut total = RetentionSweepReport::default();
        for _ in 0..50 {
            let (report, next) = store
                .retention_sweep_batch(now, cursor, 200)
                .expect("batch");
            total.absorb(&report);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(total.deleted, 5, "every eligible row is reached");
        for id in &eligible {
            assert!(!job_exists(&store, *id));
        }
        assert!(kept.iter().all(|id| job_exists(&store, *id)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_batch_rolls_back_when_an_exclusion_read_fails() {
        let store = Store::open_in_memory().expect("store");
        let id = Uuid::new_v4();
        store
            .insert_scheduled_job(&retention_job(id, false, old(), None))
            .unwrap();
        store
            .conn
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
                 ALTER TABLE harness_manager_notices RENAME TO harness_manager_notices_hidden;",
            )
            .unwrap();

        let result = store.retention_sweep_batch(Utc::now(), None, 200);

        assert!(result.is_err(), "missing exclusion source fails closed");
        assert!(job_exists(&store, id), "nothing deleted on a failed read");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_prunes_only_superseded_manager_watches_without_open_notices() {
        use rusqlite::types::Value;
        let store = Store::open_in_memory().expect("store");
        let project = Uuid::new_v4().to_string();
        insert_row(
            &store,
            "harness_manager_scopes",
            &[
                ("project_id", Value::from(project.clone())),
                ("row_version", Value::Integer(7)),
            ],
        );
        let (superseded, noticed, current, orphan) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for (job, watch_project, scope_version) in [
            (superseded, project.clone(), 6),
            (noticed, project.clone(), 6),
            (current, project, 7),
            // A project whose manager scope is gone is superseded too.
            (orphan, Uuid::new_v4().to_string(), 3),
        ] {
            store
                .insert_scheduled_job(&retention_job(job, false, old(), None))
                .unwrap();
            insert_row(
                &store,
                "harness_manager_watches",
                &[
                    ("job_id", Value::from(job.to_string())),
                    ("project_id", Value::from(watch_project)),
                    ("scope_version", Value::Integer(scope_version)),
                ],
            );
        }
        // An open (unretired, unsettled) notice still needs its transport.
        insert_reference(&store, "harness_manager_notices", "job_id", noticed);

        let report = store.retention_sweep(Utc::now(), 200, 50).expect("sweep");

        assert_eq!(report.deleted_manager_watches, 2);
        for pruned in [superseded, orphan] {
            assert!(!job_exists(&store, pruned));
            assert!(!store.is_harness_manager_watch(pruned).unwrap());
        }
        for kept in [noticed, current] {
            assert!(job_exists(&store, kept));
            assert!(store.is_harness_manager_watch(kept).unwrap());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn retention_pruned_superseded_watches_are_not_recreated_or_renotified_by_reconcile() {
        use crate::store::manager_coordinator::tests::fixture;
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;

        let store = Store::open_in_memory().expect("store");
        let (config, lead) = fixture(
            &store,
            rsi_common::harness_manager_v2::ManagerPolicyV2::default(),
        );
        store
            .reconcile_harness_manager_watches()
            .expect("reconcile");
        let watch_jobs = |version: i64| -> Vec<Uuid> {
            store
                .conn
                .prepare(
                    "SELECT job_id FROM harness_manager_watches
                      WHERE project_id=?1 AND scope_version=?2 ORDER BY job_id",
                )
                .unwrap()
                .query_map(
                    rusqlite::params![config.project_id.to_string(), version],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
                .map(|id| Uuid::parse_str(&id.unwrap()).unwrap())
                .collect()
        };
        let superseded = watch_jobs(config.row_version);
        assert!(
            !superseded.is_empty(),
            "the completed lead produced a scope-{} watch",
            config.row_version
        );

        // Widen the scope by one Epic so the scope version really advances.
        let epic = lead.parent_id.expect("lead epic");
        let mut second_epic = store.get_session(epic).unwrap().expect("epic row");
        second_epic.id = Uuid::new_v4();
        store.insert_session(&second_epic).expect("second epic");
        let current = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: config.project_id,
                session_id: config.manager_session_id,
                epic_ids: Some(vec![epic, second_epic.id]),
                expected_row_version: config.row_version,
            })
            .expect("scope bump");
        assert!(current.row_version > config.row_version);
        store
            .reconcile_harness_manager_watches()
            .expect("reconcile");
        let current_jobs = watch_jobs(current.row_version);

        // Settle history forward: retire any notice still open on the old
        // scope, then age and disable every watch job.
        let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for id in &superseded {
            store
                .conn
                .execute(
                    "UPDATE harness_manager_notices SET retired_at=?2
                      WHERE job_id=?1 AND retired_at IS NULL AND settled_at IS NULL",
                    [id.to_string(), stamp.clone()],
                )
                .unwrap();
        }
        let aged = (Utc::now() - old()).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for id in superseded.iter().chain(current_jobs.iter()) {
            store
                .conn
                .execute(
                    "UPDATE scheduled_jobs SET enabled=0,updated_at=?2,last_fired_at=?2 WHERE id=?1",
                    [id.to_string(), aged.clone()],
                )
                .unwrap();
        }
        let notices = |id: Uuid| -> i64 {
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_notices WHERE job_id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let notices_before: Vec<i64> = superseded.iter().map(|id| notices(*id)).collect();
        enable_foreign_keys(&store);

        let report = store.retention_sweep(Utc::now(), 200, 50).expect("sweep");

        assert_eq!(report.deleted_manager_watches, superseded.len());
        for id in &current_jobs {
            assert!(job_exists(&store, *id), "current-scope watch kept");
        }

        // Reconcile derives ids from the current scope only: nothing pruned
        // comes back and no new notice is recorded against a pruned id.
        store
            .reconcile_harness_manager_watches()
            .expect("reconcile");
        store
            .reconcile_harness_manager_watches()
            .expect("reconcile");
        for (index, id) in superseded.iter().enumerate() {
            assert!(!job_exists(&store, *id), "pruned watch job stays pruned");
            assert!(!store.is_harness_manager_watch(*id).unwrap());
            assert_eq!(notices(*id), notices_before[index]);
        }
        assert_eq!(watch_jobs(current.row_version), current_jobs);
    }

    /// With >= 5000 disabled rows present, the filtered read returns only
    /// enabled rows — no disabled rows leak into the hot path.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn filtered_read_excludes_disabled_at_scale() {
        let store = Store::open_in_memory().expect("in-memory store");
        let now = Utc::now();
        let past = now - chrono::Duration::minutes(30);
        // Insert 5000 disabled rows + 1 enabled row
        for i in 0..5000 {
            let id = Uuid::new_v4();
            let job = ScheduledJob {
                id,
                name: format!("old-{i}"),
                message: String::new(),
                schedule: ScheduleSpec {
                    recurrence: Recurrence::Once,
                    anchor: past,
                },
                last_fired_at: Some(past),
                next_fire_at: past,
                enabled: false,
                working_dir: None,
                provider: None,
                model: None,
                project_id: None,
                created_at: past,
                updated_at: past,
                wake_mode: WakeMode::Fresh,
                wake_session_id: None,
            };
            store.insert_scheduled_job(&job).expect("insert");
        }
        let enabled_id = Uuid::new_v4();
        let watched_session = Uuid::new_v4();
        let master = Uuid::new_v4();
        let enabled_job = ScheduledJob {
            id: enabled_id,
            name: "enabled-on-terminal".into(),
            message: String::new(),
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
            wake_mode: WakeMode::OnTerminal(watched_session),
            wake_session_id: Some(master),
        };
        store
            .insert_scheduled_job(&enabled_job)
            .expect("insert enabled");
        let total: i64 = store
            .conn
            .query_row("SELECT count(*) FROM scheduled_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 5001, "5000 disabled + 1 enabled");
        // list_enabled_terminal_watches returns only the enabled on_terminal row
        let enabled_watches = store.list_enabled_terminal_watches().expect("list");
        assert_eq!(
            enabled_watches.len(),
            1,
            "only the enabled on_terminal row returned"
        );
        assert_eq!(enabled_watches[0].id, enabled_id);
        // Verify all returned rows are enabled and on_terminal
        for w in &enabled_watches {
            assert!(w.enabled, "all returned rows are enabled");
            assert!(
                matches!(w.wake_mode, WakeMode::OnTerminal(_)),
                "all returned rows are on_terminal"
            );
        }
    }
}

/// K2 retry bound: the 8th consecutive retryable refusal exhausts the wake.
pub(crate) const CONTINUATION_RETRY_MAX_ATTEMPTS: u32 = 7;

/// Backoff before attempt `attempts` (1-based): 30s doubling, capped at 10
/// minutes. Shared with the transient-failure heal (#1015).
pub(crate) fn continuation_backoff_seconds(attempts: u32) -> i64 {
    (30_i64 << attempts.saturating_sub(1).min(5)).min(600)
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod cancel_wake_tests {
    use super::*;
    use rsi_common::types::{ScheduleSpec, SessionStatus};

    fn owned_job(name: &str, mode: WakeMode, owner: Option<Uuid>) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: name.to_string(),
            message: "continue".to_string(),
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
            wake_mode: mode,
            wake_session_id: owner,
        }
    }

    fn enabled(store: &Store, id: Uuid) -> bool {
        store
            .get_scheduled_job(&id)
            .unwrap()
            .expect("cancelled wakes are disabled, never deleted")
            .enabled
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cancelling_an_own_resume_wake_clears_the_succession_recovery_owner_gate() {
        let store = Store::open_in_memory().unwrap();
        let manager = Uuid::new_v4();
        let mut session = crate::session::agent_verbs::tests::test_session(
            manager,
            std::path::PathBuf::from("/var/tmp/cancel-wake"),
        );
        session.status = SessionStatus::Completed;
        store.insert_session(&session).unwrap();
        let job = owned_job("safety-net", WakeMode::Resume, Some(manager));
        store.insert_scheduled_job(&job).unwrap();

        let refused = store.manager_action_human_gate(manager).unwrap_err();
        assert!(
            matches!(&refused, DaemonError::InvalidParam(code) if code == "manager_v2_human_or_recovery_owner"),
            "{refused:?}"
        );

        let cancelled = store
            .cancel_owned_scheduled_jobs(manager, Some(job.id), None)
            .unwrap();
        assert_eq!(cancelled, vec![job.id]);
        assert!(!enabled(&store, job.id));
        store.manager_action_human_gate(manager).unwrap();

        // Cancelling again is an idempotent no-op on an owned, disabled row.
        assert!(
            store
                .cancel_owned_scheduled_jobs(manager, Some(job.id), None)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cancel_refuses_another_sessions_job_by_id_and_by_name() {
        let store = Store::open_in_memory().unwrap();
        let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());
        let job = owned_job("shared-name", WakeMode::Resume, Some(theirs));
        store.insert_scheduled_job(&job).unwrap();

        for error in [
            store
                .cancel_owned_scheduled_jobs(mine, Some(job.id), None)
                .unwrap_err(),
            store
                .cancel_owned_scheduled_jobs(mine, None, Some("shared-name"))
                .unwrap_err(),
        ] {
            assert!(
                matches!(&error, DaemonError::InvalidParam(code) if code.starts_with("wake_not_found")),
                "{error:?}"
            );
        }
        assert!(enabled(&store, job.id));
        // No selector never matches anything, even the caller's own jobs.
        assert!(
            store
                .cancel_owned_scheduled_jobs(theirs, None, None)
                .is_err()
        );
        assert!(enabled(&store, job.id));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cancel_by_name_disables_every_enabled_own_job_of_that_name() {
        let store = Store::open_in_memory().unwrap();
        let owner = Uuid::new_v4();
        let first = owned_job("net", WakeMode::Resume, Some(owner));
        let second = owned_job("net", WakeMode::AgentFresh, Some(owner));
        let other_name = owned_job("keep", WakeMode::Resume, Some(owner));
        for job in [&first, &second, &other_name] {
            store.insert_scheduled_job(job).unwrap();
        }
        let mut cancelled = store
            .cancel_owned_scheduled_jobs(owner, None, Some("net"))
            .unwrap();
        cancelled.sort();
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(cancelled, expected);
        assert!(!enabled(&store, first.id));
        assert!(!enabled(&store, second.id));
        assert!(enabled(&store, other_name.id));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn cancel_refuses_the_daemon_owned_program_guard() {
        let store = Store::open_in_memory().unwrap();
        let owner = Uuid::new_v4();
        store
            .insert_session(&crate::session::agent_verbs::tests::test_session(
                owner,
                std::path::PathBuf::from("/var/tmp/cancel-wake-guard"),
            ))
            .unwrap();
        let mut guard = owned_job("program-guard", WakeMode::Resume, Some(owner));
        guard.id =
            crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(
                owner,
            );
        store.insert_scheduled_job(&guard).unwrap();
        let error = store
            .cancel_owned_scheduled_jobs(owner, Some(guard.id), None)
            .unwrap_err();
        assert!(
            matches!(&error, DaemonError::InvalidParam(code) if code.starts_with("wake_protected")),
            "{error:?}"
        );
        assert!(enabled(&store, guard.id));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn agent_cancel_refuses_the_internal_process_wake_but_the_daemon_withdraws_it() {
        let store = Store::open_in_memory().unwrap();
        let owner = Uuid::new_v4();
        store
            .insert_session(&crate::session::agent_verbs::tests::test_session(
                owner,
                std::path::PathBuf::from("/var/tmp/cancel-wake-internal"),
            ))
            .unwrap();
        let name = format!("{BACKGROUND_PROCESS_WAKE_PREFIX}{owner}");
        let wake = owned_job(&name, WakeMode::Resume, Some(owner));
        store.insert_scheduled_job(&wake).unwrap();

        let by_id = store
            .cancel_owned_scheduled_jobs(owner, Some(wake.id), None)
            .unwrap_err();
        assert!(
            matches!(&by_id, DaemonError::InvalidParam(code) if code.starts_with("wake_protected")),
            "{by_id:?}"
        );
        let by_name = store
            .cancel_owned_scheduled_jobs(owner, None, Some(&name))
            .unwrap_err();
        assert!(
            matches!(&by_name, DaemonError::InvalidParam(code) if code.starts_with("wake_protected")),
            "{by_name:?}"
        );
        assert!(enabled(&store, wake.id));
        let listed = store.list_owned_scheduled_jobs(owner, false, 8).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].1, "the internal wake is listed as protected");

        // The registry can still replace its own row (coalescing) and withdraw it.
        let replacement = owned_job(&name, WakeMode::Resume, Some(owner));
        let replaced = store
            .insert_scheduled_job_replacing_name(owner, &replacement)
            .unwrap();
        assert_eq!(replaced, vec![wake.id]);
        assert!(!enabled(&store, wake.id));
        assert!(enabled(&store, replacement.id));
        let withdrawn = store.cancel_internal_process_wake(owner).unwrap();
        assert_eq!(withdrawn, vec![replacement.id]);
        assert!(!enabled(&store, replacement.id));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn insert_replacing_name_keeps_one_enabled_job_per_name_per_session() {
        let store = Store::open_in_memory().unwrap();
        let (owner, other) = (Uuid::new_v4(), Uuid::new_v4());
        let first = owned_job("net", WakeMode::Resume, Some(owner));
        let unrelated_name = owned_job("other", WakeMode::Resume, Some(owner));
        let other_session = owned_job("net", WakeMode::Resume, Some(other));
        for job in [&first, &unrelated_name, &other_session] {
            store.insert_scheduled_job(job).unwrap();
        }
        let second = owned_job("net", WakeMode::Resume, Some(owner));
        let replaced = store
            .insert_scheduled_job_replacing_name(owner, &second)
            .unwrap();
        assert_eq!(replaced, vec![first.id]);
        assert!(!enabled(&store, first.id));
        assert!(enabled(&store, second.id));
        assert!(enabled(&store, unrelated_name.id));
        assert!(enabled(&store, other_session.id));
    }
}
