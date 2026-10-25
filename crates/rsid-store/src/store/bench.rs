//! The bench run store (#3178, program #3135 M3 S4): `bench_runs` and `bench_tasks`.
//!
//! A run is one admitted benchmark run with its pins (config digest, dataset
//! commit, task set with each task's image digests, model, effort, budget,
//! per-task USD cap, concurrency). A task row is one trial of it. Rows are never
//! deleted and only move forward (see the bench migration). Usage is read from the increment
//! SQL (`cost_increment_for_ids_sql!`), never from the session-cumulative rows.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::bench::{BenchRunState, BenchTaskState, MAX_CONCURRENCY};
use rsi_common::bench_profile::LoopKnobs;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One task of a run's pinned task set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchRunTask {
    pub task: String,
    /// `sha256:<64 hex>` image digests the task's container may carry.
    pub pinned_digests: Vec<String>,
}

/// A run to record.
#[derive(Debug, Clone, PartialEq)]
pub struct NewBenchRun {
    pub id: Uuid,
    pub project_id: Uuid,
    pub job_id: Option<String>,
    pub config_path: String,
    pub config_digest: String,
    pub dataset_commit: String,
    pub docker_host: String,
    pub model: String,
    pub effort: String,
    pub budget_usd: f64,
    pub per_task_cap_usd: f64,
    pub concurrency: u32,
    pub tasks: Vec<BenchRunTask>,
    pub loop_knobs: LoopKnobs,
}

/// A stored run.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchRunRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub job_id: Option<String>,
    pub config_path: String,
    pub config_digest: String,
    pub dataset_commit: String,
    pub docker_host: String,
    pub model: String,
    pub effort: String,
    pub budget_usd: f64,
    pub per_task_cap_usd: f64,
    pub concurrency: u32,
    pub tasks: Vec<BenchRunTask>,
    pub loop_knobs: LoopKnobs,
    pub state: BenchRunState,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl BenchRunRow {
    /// The pinned entry for `task`, if the run holds it.
    #[must_use]
    pub fn task(&self, task: &str) -> Option<&BenchRunTask> {
        self.tasks.iter().find(|entry| entry.task == task)
    }
}

/// A trial to claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBenchTask {
    pub trial: String,
    pub task: String,
    pub compose_project: String,
    pub container: String,
    pub workdir: String,
    pub user: Option<String>,
    pub agent_timeout_sec: u64,
    /// Digest of the whole request; a repeated start with another one conflicts.
    pub request_digest: String,
}

/// #4013: a finished bench trial session still at the project root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UngroupedBenchSession {
    pub session_id: Uuid,
    /// The session's project; `None` for a session without one.
    pub project_id: Option<Uuid>,
    /// The run that launched it; `None` when no run row binds the session.
    pub run_id: Option<Uuid>,
}

/// A stored trial.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchTaskRow {
    pub run_id: Uuid,
    pub trial: String,
    pub task: String,
    pub attempt: u32,
    pub session_id: Option<Uuid>,
    pub compose_project: String,
    pub container: String,
    pub workdir: String,
    pub user: Option<String>,
    pub agent_timeout_sec: u64,
    pub request_digest: String,
    pub state: BenchTaskState,
    pub stop_reason: Option<String>,
    pub cost_usd: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub wall_ms: Option<u64>,
    pub harness_build_digest: Option<String>,
    pub trace_path: Option<String>,
    pub trace_sha256: Option<String>,
    pub route_verdict: Option<String>,
    pub route_violations: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// The result of claiming a trial.
#[derive(Debug, Clone, PartialEq)]
pub enum BenchClaim {
    /// A new `starting` row.
    Claimed(BenchTaskRow),
    /// The trial id already exists (start is idempotent per (run, trial)).
    Existing(BenchTaskRow),
    /// The run is not active.
    RunNotActive,
    /// The run already has its concurrency cap of live trials.
    ConcurrencyFull,
}

/// What settles a trial.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchSettlement {
    pub state: BenchTaskState,
    pub stop_reason: Option<String>,
    pub cost_usd: Option<f64>,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub wall_ms: u64,
    pub harness_build_digest: Option<String>,
    pub trace_sha256: Option<String>,
    pub route_verdict: Option<String>,
    pub route_violations: Vec<String>,
    pub finished_at: DateTime<Utc>,
}

/// One session's usage, from the increment SQL.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BenchSessionUsage {
    /// Admitted, completed model calls.
    pub calls: u64,
    /// Sum of the priced calls' own cost; `None` when no call has a known cost.
    pub cost_usd: Option<f64>,
    /// The largest single call's own cost.
    pub max_call_usd: f64,
    /// Completed calls with no known cost (spend unknown).
    pub unpriced_calls: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_stamp(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|e| DaemonError::Store(format!("corrupt bench timestamp: {e}")))
}

fn invalid(reason: &str) -> DaemonError {
    DaemonError::InvalidParam(format!("bench_run_invalid:{reason}"))
}

fn digest_ok(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl NewBenchRun {
    fn validate(&self) -> Result<()> {
        if !self.budget_usd.is_finite() || self.budget_usd <= 0.0 {
            return Err(invalid("budget"));
        }
        if !self.per_task_cap_usd.is_finite()
            || self.per_task_cap_usd <= 0.0
            || self.per_task_cap_usd > self.budget_usd
        {
            return Err(invalid("per_task_cap"));
        }
        if self.concurrency == 0 || self.concurrency > MAX_CONCURRENCY {
            return Err(invalid("concurrency"));
        }
        for text in [
            &self.config_path,
            &self.config_digest,
            &self.dataset_commit,
            &self.docker_host,
            &self.model,
            &self.effort,
        ] {
            if text.trim().is_empty() {
                return Err(invalid("empty_field"));
            }
        }
        if self.tasks.is_empty() {
            return Err(invalid("no_tasks"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for task in &self.tasks {
            if task.task.is_empty()
                || !seen.insert(task.task.as_str())
                || task.pinned_digests.is_empty()
                || !task.pinned_digests.iter().all(|d| digest_ok(d))
            {
                return Err(invalid("task_set"));
            }
        }
        self.loop_knobs.validate().map_err(invalid)?;
        Ok(())
    }
}

macro_rules! run_columns {
    () => {
        "id, project_id, job_id, config_path, config_digest, dataset_commit, \
         docker_host, model, effort, budget_usd, per_task_cap_usd, concurrency, tasks_json, \
         loop_knobs_json, state, created_at, finished_at"
    };
}

macro_rules! task_columns {
    () => {
        "run_id, trial, task, attempt, session_id, compose_project, container, \
         workdir, user, agent_timeout_sec, request_digest, state, stop_reason, cost_usd, \
         prompt_tokens, cached_tokens, output_tokens, reasoning_tokens, wall_ms, \
         harness_build_digest, trace_path, trace_sha256, route_verdict, route_violations_json, \
         started_at, finished_at"
    };
}

const SELECT_RUN_BY_ID: &str = concat!("SELECT ", run_columns!(), " FROM bench_runs WHERE id = ?1");
const SELECT_ACTIVE_RUNS: &str = concat!(
    "SELECT ",
    run_columns!(),
    " FROM bench_runs WHERE state = 'active' ORDER BY created_at, id"
);
const SELECT_TASK_BY_TRIAL: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM bench_tasks WHERE run_id = ?1 AND trial = ?2"
);
const SELECT_TASK_BY_SESSION: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM bench_tasks WHERE session_id = ?1"
);
const SELECT_RUN_TASKS: &str = concat!(
    "SELECT ",
    task_columns!(),
    " FROM bench_tasks WHERE run_id = ?1 ORDER BY started_at, trial"
);

fn read_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRaw> {
    Ok(RunRaw {
        id: row.get(0)?,
        project_id: row.get(1)?,
        job_id: row.get(2)?,
        config_path: row.get(3)?,
        config_digest: row.get(4)?,
        dataset_commit: row.get(5)?,
        docker_host: row.get(6)?,
        model: row.get(7)?,
        effort: row.get(8)?,
        budget_usd: row.get(9)?,
        per_task_cap_usd: row.get(10)?,
        concurrency: row.get(11)?,
        tasks_json: row.get(12)?,
        loop_knobs_json: row.get(13)?,
        state: row.get(14)?,
        created_at: row.get(15)?,
        finished_at: row.get(16)?,
    })
}

struct RunRaw {
    id: String,
    project_id: String,
    job_id: Option<String>,
    config_path: String,
    config_digest: String,
    dataset_commit: String,
    docker_host: String,
    model: String,
    effort: String,
    budget_usd: f64,
    per_task_cap_usd: f64,
    concurrency: i64,
    tasks_json: String,
    loop_knobs_json: String,
    state: String,
    created_at: String,
    finished_at: Option<String>,
}

fn corrupt(what: &str) -> DaemonError {
    DaemonError::Store(format!("corrupt bench row: {what}"))
}

impl RunRaw {
    fn into_row(self) -> Result<BenchRunRow> {
        Ok(BenchRunRow {
            id: Uuid::parse_str(&self.id).map_err(|_| corrupt("run id"))?,
            project_id: Uuid::parse_str(&self.project_id).map_err(|_| corrupt("project id"))?,
            job_id: self.job_id,
            config_path: self.config_path,
            config_digest: self.config_digest,
            dataset_commit: self.dataset_commit,
            docker_host: self.docker_host,
            model: self.model,
            effort: self.effort,
            budget_usd: self.budget_usd,
            per_task_cap_usd: self.per_task_cap_usd,
            concurrency: u32::try_from(self.concurrency).map_err(|_| corrupt("concurrency"))?,
            tasks: serde_json::from_str(&self.tasks_json).map_err(|_| corrupt("tasks"))?,
            loop_knobs: serde_json::from_str(&self.loop_knobs_json)
                .map_err(|_| corrupt("loop knobs"))?,
            state: BenchRunState::parse(&self.state).ok_or_else(|| corrupt("run state"))?,
            created_at: parse_stamp(&self.created_at)?,
            finished_at: self.finished_at.as_deref().map(parse_stamp).transpose()?,
        })
    }
}

fn read_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRaw> {
    Ok(TaskRaw {
        run_id: row.get(0)?,
        trial: row.get(1)?,
        task: row.get(2)?,
        attempt: row.get(3)?,
        session_id: row.get(4)?,
        compose_project: row.get(5)?,
        container: row.get(6)?,
        workdir: row.get(7)?,
        user: row.get(8)?,
        agent_timeout_sec: row.get(9)?,
        request_digest: row.get(10)?,
        state: row.get(11)?,
        stop_reason: row.get(12)?,
        cost_usd: row.get(13)?,
        prompt_tokens: row.get(14)?,
        cached_tokens: row.get(15)?,
        output_tokens: row.get(16)?,
        reasoning_tokens: row.get(17)?,
        wall_ms: row.get(18)?,
        harness_build_digest: row.get(19)?,
        trace_path: row.get(20)?,
        trace_sha256: row.get(21)?,
        route_verdict: row.get(22)?,
        route_violations_json: row.get(23)?,
        started_at: row.get(24)?,
        finished_at: row.get(25)?,
    })
}

struct TaskRaw {
    run_id: String,
    trial: String,
    task: String,
    attempt: i64,
    session_id: Option<String>,
    compose_project: String,
    container: String,
    workdir: String,
    user: Option<String>,
    agent_timeout_sec: i64,
    request_digest: String,
    state: String,
    stop_reason: Option<String>,
    cost_usd: Option<f64>,
    prompt_tokens: Option<i64>,
    cached_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    wall_ms: Option<i64>,
    harness_build_digest: Option<String>,
    trace_path: Option<String>,
    trace_sha256: Option<String>,
    route_verdict: Option<String>,
    route_violations_json: Option<String>,
    started_at: String,
    finished_at: Option<String>,
}

fn unsigned(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok())
}

impl TaskRaw {
    fn into_row(self) -> Result<BenchTaskRow> {
        Ok(BenchTaskRow {
            run_id: Uuid::parse_str(&self.run_id).map_err(|_| corrupt("run id"))?,
            trial: self.trial,
            task: self.task,
            attempt: u32::try_from(self.attempt).map_err(|_| corrupt("attempt"))?,
            session_id: self
                .session_id
                .as_deref()
                .map(Uuid::parse_str)
                .transpose()
                .map_err(|_| corrupt("session id"))?,
            compose_project: self.compose_project,
            container: self.container,
            workdir: self.workdir,
            user: self.user,
            agent_timeout_sec: u64::try_from(self.agent_timeout_sec)
                .map_err(|_| corrupt("agent timeout"))?,
            request_digest: self.request_digest,
            state: BenchTaskState::parse(&self.state).ok_or_else(|| corrupt("task state"))?,
            stop_reason: self.stop_reason,
            cost_usd: self.cost_usd,
            prompt_tokens: unsigned(self.prompt_tokens),
            cached_tokens: unsigned(self.cached_tokens),
            output_tokens: unsigned(self.output_tokens),
            reasoning_tokens: unsigned(self.reasoning_tokens),
            wall_ms: unsigned(self.wall_ms),
            harness_build_digest: self.harness_build_digest,
            trace_path: self.trace_path,
            trace_sha256: self.trace_sha256,
            route_verdict: self.route_verdict,
            route_violations: self
                .route_violations_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| corrupt("route violations"))?
                .unwrap_or_default(),
            started_at: parse_stamp(&self.started_at)?,
            finished_at: self.finished_at.as_deref().map(parse_stamp).transpose()?,
        })
    }
}

impl Store {
    /// Record an admitted run (state `active`).
    ///
    /// # Errors
    /// `bench_run_invalid:<reason>` for a malformed run, or a store failure
    /// (including a repeated id).
    pub fn insert_bench_run(&self, run: &NewBenchRun, now: DateTime<Utc>) -> Result<()> {
        run.validate()?;
        self.conn.execute(
            "INSERT INTO bench_runs(id, project_id, job_id, config_path, config_digest, \
             dataset_commit, docker_host, model, effort, budget_usd, per_task_cap_usd, \
             concurrency, tasks_json, loop_knobs_json, state, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'active', ?15)",
            params![
                run.id.to_string(),
                run.project_id.to_string(),
                run.job_id,
                run.config_path,
                run.config_digest,
                run.dataset_commit,
                run.docker_host,
                run.model,
                run.effort,
                run.budget_usd,
                run.per_task_cap_usd,
                i64::from(run.concurrency),
                serde_json::to_string(&run.tasks)?,
                serde_json::to_string(&run.loop_knobs)?,
                stamp(now),
            ],
        )?;
        Ok(())
    }

    /// The run `id`, if any.
    pub fn get_bench_run(&self, id: Uuid) -> Result<Option<BenchRunRow>> {
        self.conn
            .query_row(SELECT_RUN_BY_ID, [id.to_string()], read_run)
            .optional()?
            .map(RunRaw::into_row)
            .transpose()
    }

    /// Every active run, oldest first.
    pub fn list_active_bench_runs(&self) -> Result<Vec<BenchRunRow>> {
        let mut stmt = self.conn.prepare(SELECT_ACTIVE_RUNS)?;
        let raws = stmt
            .query_map([], read_run)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        raws.into_iter().map(RunRaw::into_row).collect()
    }

    /// #4013: finished bench trial sessions still parented at the project root,
    /// oldest first, at most `limit`. A trial qualifies when its session is an
    /// ended `is_eval` Standard session with no parent and either belongs to a
    /// run that is no longer active (`run_id` is set) or carries no run row but
    /// is titled `bench ...` (`run_id` is `None`; it goes to the project's
    /// Benchmarks group). A session of a still-active run is never listed.
    pub fn list_ungrouped_finished_bench_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<UngroupedBenchSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.project_id, bt.run_id \
             FROM sessions s \
             LEFT JOIN bench_tasks bt ON bt.session_id = s.id \
             LEFT JOIN bench_runs r ON r.id = bt.run_id \
             WHERE s.parent_id IS NULL AND s.is_eval = 1 \
               AND s.session_kind = 'Standard' \
               AND s.status IN ('Completed','Failed','Interrupted','Archived') \
               AND ((bt.run_id IS NOT NULL AND r.state <> 'active') \
                    OR (bt.run_id IS NULL AND s.title LIKE 'bench %')) \
             ORDER BY s.created_at, s.id LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, project, run)| {
                Some(UngroupedBenchSession {
                    session_id: Uuid::parse_str(&id).ok()?,
                    project_id: project.and_then(|p| Uuid::parse_str(&p).ok()),
                    run_id: run.and_then(|r| Uuid::parse_str(&r).ok()),
                })
            })
            .collect())
    }

    /// #4013: set the parent of a finished root-level `is_eval` session to
    /// `group`. Only `parent_id` changes; the guard makes a repeat, or a session
    /// that was parented or revived since the listing, a no-op (`false`).
    pub fn parent_ungrouped_bench_session(&self, session: Uuid, group: Uuid) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE sessions SET parent_id = ?2 \
             WHERE id = ?1 AND parent_id IS NULL AND is_eval = 1 \
               AND status IN ('Completed','Failed','Interrupted','Archived')",
            params![session.to_string(), group.to_string()],
        )?;
        Ok(changed == 1)
    }

    /// End an active run. Returns whether it was active.
    pub fn end_bench_run(
        &self,
        id: Uuid,
        state: BenchRunState,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        if state == BenchRunState::Active {
            return Err(invalid("end_state"));
        }
        let changed = self.conn.execute(
            "UPDATE bench_runs SET state = ?2, finished_at = ?3 WHERE id = ?1 AND state = 'active'",
            params![id.to_string(), state.as_str(), stamp(now)],
        )?;
        Ok(changed == 1)
    }

    /// Claim a trial: one transaction checks the run is active, answers an
    /// existing (run, trial) as is, enforces the concurrency cap and inserts the
    /// `starting` row with the next attempt number of its task.
    pub fn claim_bench_task(
        &self,
        run_id: Uuid,
        claim: &NewBenchTask,
        now: DateTime<Utc>,
    ) -> Result<BenchClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let run = run_id.to_string();
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM bench_runs WHERE id = ?1",
                [&run],
                |row| row.get(0),
            )
            .optional()?;
        if state.as_deref() != Some("active") {
            return Ok(BenchClaim::RunNotActive);
        }
        if let Some(existing) = tx
            .query_row(SELECT_TASK_BY_TRIAL, params![run, claim.trial], read_task)
            .optional()?
        {
            return Ok(BenchClaim::Existing(existing.into_row()?));
        }
        let (live, concurrency): (i64, i64) = tx.query_row(
            "SELECT (SELECT COUNT(*) FROM bench_tasks WHERE run_id = ?1 \
                       AND state IN ('starting','running')), concurrency \
             FROM bench_runs WHERE id = ?1",
            [&run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if live >= concurrency {
            return Ok(BenchClaim::ConcurrencyFull);
        }
        let attempt: i64 = tx.query_row(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM bench_tasks WHERE run_id = ?1 AND task = ?2",
            params![run, claim.task],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO bench_tasks(run_id, trial, task, attempt, compose_project, container, \
             workdir, user, agent_timeout_sec, request_digest, state, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'starting', ?11)",
            params![
                run,
                claim.trial,
                claim.task,
                attempt,
                claim.compose_project,
                claim.container,
                claim.workdir,
                claim.user,
                i64::try_from(claim.agent_timeout_sec).unwrap_or(i64::MAX),
                claim.request_digest,
                stamp(now),
            ],
        )?;
        let row = tx
            .query_row(SELECT_TASK_BY_TRIAL, params![run, claim.trial], read_task)?
            .into_row()?;
        tx.commit()?;
        Ok(BenchClaim::Claimed(row))
    }

    /// Bind a claimed trial to its launched session: `starting` -> `running`.
    /// Returns whether the row moved (a trial an interrupt or cancel already
    /// settled stays as it is).
    pub fn bind_bench_task_session(
        &self,
        run_id: Uuid,
        trial: &str,
        session_id: Uuid,
        trace_path: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE bench_tasks SET session_id = ?3, trace_path = ?4, state = 'running' \
             WHERE run_id = ?1 AND trial = ?2 AND state = 'starting'",
            params![
                run_id.to_string(),
                trial,
                session_id.to_string(),
                trace_path
            ],
        )?;
        Ok(changed == 1)
    }

    /// The trial `trial` of `run_id`, if any.
    pub fn get_bench_task(&self, run_id: Uuid, trial: &str) -> Result<Option<BenchTaskRow>> {
        self.conn
            .query_row(
                SELECT_TASK_BY_TRIAL,
                params![run_id.to_string(), trial],
                read_task,
            )
            .optional()?
            .map(TaskRaw::into_row)
            .transpose()
    }

    /// The trial bound to `session_id`, if any.
    pub fn get_bench_task_by_session(&self, session_id: Uuid) -> Result<Option<BenchTaskRow>> {
        self.conn
            .query_row(SELECT_TASK_BY_SESSION, [session_id.to_string()], read_task)
            .optional()?
            .map(TaskRaw::into_row)
            .transpose()
    }

    /// Every trial of a run, in start order.
    pub fn list_bench_tasks(&self, run_id: Uuid) -> Result<Vec<BenchTaskRow>> {
        let mut stmt = self.conn.prepare(SELECT_RUN_TASKS)?;
        let raws = stmt
            .query_map([run_id.to_string()], read_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        raws.into_iter().map(TaskRaw::into_row).collect()
    }

    /// The sessions of a run's trials.
    pub fn bench_run_session_ids(&self, run_id: Uuid) -> Result<Vec<Uuid>> {
        Ok(self
            .list_bench_tasks(run_id)?
            .into_iter()
            .filter_map(|task| task.session_id)
            .collect())
    }

    /// Settle a trial. Only a live row (`starting`/`running`) takes a new state;
    /// a settled row only gains the fields it lacks while keeping its state
    /// (a cancelled trial's session may end after the cancel, and the usage and
    /// verdict it leaves are still recorded). Returns whether a row changed.
    pub fn settle_bench_task(
        &self,
        run_id: Uuid,
        trial: &str,
        settlement: &BenchSettlement,
    ) -> Result<bool> {
        if settlement.state.is_live() {
            return Err(invalid("settle_state"));
        }
        let violations = serde_json::to_string(&settlement.route_violations)?;
        let changed = self.conn.execute(
            "UPDATE bench_tasks SET \
                state = CASE WHEN state IN ('starting','running') THEN ?3 ELSE state END, \
                stop_reason = COALESCE(stop_reason, ?4), \
                cost_usd = ?5, prompt_tokens = ?6, cached_tokens = ?7, output_tokens = ?8, \
                reasoning_tokens = ?9, wall_ms = ?10, \
                harness_build_digest = COALESCE(?11, harness_build_digest), \
                trace_sha256 = COALESCE(trace_sha256, ?12), \
                route_verdict = ?13, route_violations_json = ?14, \
                finished_at = COALESCE(finished_at, ?15) \
             WHERE run_id = ?1 AND trial = ?2",
            params![
                run_id.to_string(),
                trial,
                settlement.state.as_str(),
                settlement.stop_reason,
                settlement.cost_usd,
                i64::try_from(settlement.prompt_tokens).unwrap_or(i64::MAX),
                i64::try_from(settlement.cached_tokens).unwrap_or(i64::MAX),
                i64::try_from(settlement.output_tokens).unwrap_or(i64::MAX),
                i64::try_from(settlement.reasoning_tokens).unwrap_or(i64::MAX),
                i64::try_from(settlement.wall_ms).unwrap_or(i64::MAX),
                settlement.harness_build_digest,
                settlement.trace_sha256,
                settlement.route_verdict,
                violations,
                stamp(settlement.finished_at),
            ],
        )?;
        Ok(changed == 1)
    }

    /// A daemon restart: every live trial becomes `infra_interrupted`. They are
    /// never resumed. Returns each interrupted trial's run, id and session.
    pub fn interrupt_live_bench_tasks(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(Uuid, String, Option<Uuid>)>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut stmt = tx.prepare(
            "SELECT run_id, trial, session_id FROM bench_tasks \
             WHERE state IN ('starting','running') ORDER BY started_at, trial",
        )?;
        let live = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        tx.execute(
            "UPDATE bench_tasks SET state = 'infra_interrupted', \
                stop_reason = COALESCE(stop_reason, 'infra_interrupted'), finished_at = ?1 \
             WHERE state IN ('starting','running')",
            [stamp(now)],
        )?;
        tx.commit()?;
        live.into_iter()
            .map(|(run, trial, session)| {
                Ok((
                    Uuid::parse_str(&run).map_err(|_| corrupt("run id"))?,
                    trial,
                    session
                        .as_deref()
                        .map(Uuid::parse_str)
                        .transpose()
                        .map_err(|_| corrupt("session id"))?,
                ))
            })
            .collect()
    }

    /// One session's usage from the increment SQL: each invocation's OWN cost
    /// (a session-cumulative row is differenced against its predecessor), never
    /// a SUM of the stored column.
    pub fn bench_session_usage(&self, session_id: Uuid) -> Result<BenchSessionUsage> {
        let row = self.conn.query_row(
            concat!(
                "WITH ids(id) AS (SELECT ?1)
                 SELECT COUNT(*),
                        SUM(CASE WHEN ci.cost_increment_usd IS NOT NULL
                                  AND ci.cost_increment_usd >= 0
                                 THEN ci.cost_increment_usd END),
                        MAX(CASE WHEN ci.cost_increment_usd IS NOT NULL
                                  AND ci.cost_increment_usd >= 0
                                 THEN ci.cost_increment_usd END),
                        SUM(CASE WHEN ci.cost_increment_usd IS NULL THEN 1 ELSE 0 END),
                        COALESCE(SUM(mi.input_tokens), 0),
                        COALESCE(SUM(mi.cache_read_tokens), 0),
                        COALESCE(SUM(mi.output_tokens), 0),
                        COALESCE(SUM(mi.reasoning_tokens), 0)
                 FROM (",
                crate::model_control::cost::cost_increment_for_ids_sql!(),
                ") ci JOIN model_invocations mi ON mi.id = ci.invocation_id
                 WHERE ci.admission_status = 'admitted' AND mi.status = 'completed'"
            ),
            [session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<f64>>(1)?,
                    row.get::<_, Option<f64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )?;
        let count = |value: i64| u64::try_from(value).unwrap_or(0);
        Ok(BenchSessionUsage {
            calls: count(row.0),
            cost_usd: row.1,
            max_call_usd: row.2.unwrap_or(0.0),
            unpriced_calls: count(row.3.unwrap_or(0)),
            prompt_tokens: count(row.4),
            cached_tokens: count(row.5),
            output_tokens: count(row.6),
            reasoning_tokens: count(row.7),
        })
    }
}

impl Store {
    /// Test fixture: one settled, admitted model invocation of `session_id`
    /// with the given cost and token counts, written without admission. `n`
    /// keeps the row ids distinct within a store.
    #[cfg(any(test, feature = "test-seam"))]
    pub fn insert_bench_invocation_for_test(
        &self,
        session_id: Uuid,
        n: usize,
        purpose: &str,
        cost_usd: Option<f64>,
        tokens: (i64, i64, i64, i64),
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO model_invocations(id, purpose, invocation_kind, foreground,
                                           paid_risk, admission_status, status,
                                           trigger_source, session_id, created_at,
                                           estimated_cost_usd, input_tokens,
                                           cache_read_tokens, output_tokens, reasoning_tokens)
             VALUES(?1, ?2, 'session_lifecycle', 'foreground', 'paid_capable',
                    'admitted', 'completed', 'test', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                format!("bench-test-{session_id}-{n}"),
                purpose,
                session_id.to_string(),
                format!("2026-10-19T00:00:{:02}.000000000Z", n % 60),
                cost_usd,
                tokens.0,
                tokens.1,
                tokens.2,
                tokens.3,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    pub(crate) fn run(id: Uuid) -> NewBenchRun {
        NewBenchRun {
            id,
            project_id: Uuid::new_v4(),
            job_id: Some("job-1".into()),
            config_path: "eval/bench/tb2/runs/smoke.toml".into(),
            config_digest: digest('c'),
            dataset_commit: "2fd12b88aafdd04a52c298e3940bcb189f9766d6".into(),
            docker_host: "ssh://arch-laptop".into(),
            model: "deepseek/deepseek-v4.1-flash".into(),
            effort: "high".into(),
            budget_usd: 1.0,
            per_task_cap_usd: 0.4,
            concurrency: 2,
            tasks: vec![
                BenchRunTask {
                    task: "fix-git".into(),
                    pinned_digests: vec![digest('a')],
                },
                BenchRunTask {
                    task: "regex-log".into(),
                    pinned_digests: vec![digest('b')],
                },
            ],
            loop_knobs: LoopKnobs::default(),
        }
    }

    fn claim(trial: &str, task: &str) -> NewBenchTask {
        NewBenchTask {
            trial: trial.into(),
            task: task.into(),
            compose_project: format!("{trial}-proj"),
            container: format!("{trial}-main-1"),
            workdir: "/app".into(),
            user: None,
            agent_timeout_sec: 900,
            request_digest: digest('d'),
        }
    }

    fn seed_session(store: &Store) -> Uuid {
        let id = Uuid::new_v4();
        let session = crate::test_support::test_session(id, std::path::PathBuf::from("/tmp/bench"));
        store.insert_session(&session).unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_run_round_trips_and_malformed_runs_are_refused() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        store.insert_bench_run(&run(id), Utc::now()).unwrap();
        let stored = store.get_bench_run(id).unwrap().unwrap();
        assert_eq!(stored.state, BenchRunState::Active);
        assert_eq!(stored.model, "deepseek/deepseek-v4.1-flash");
        assert_eq!(
            stored.task("fix-git").unwrap().pinned_digests,
            [digest('a')]
        );
        assert_eq!(store.list_active_bench_runs().unwrap().len(), 1);

        let mut bad = run(Uuid::new_v4());
        bad.per_task_cap_usd = 2.0;
        assert!(store.insert_bench_run(&bad, Utc::now()).is_err());
        let mut bad = run(Uuid::new_v4());
        bad.tasks[0].pinned_digests = vec!["latest".into()];
        assert!(store.insert_bench_run(&bad, Utc::now()).is_err());
        let mut bad = run(Uuid::new_v4());
        bad.concurrency = 0;
        assert!(store.insert_bench_run(&bad, Utc::now()).is_err());

        assert!(
            store
                .end_bench_run(id, BenchRunState::Finished, Utc::now())
                .unwrap()
        );
        assert!(store.list_active_bench_runs().unwrap().is_empty());
        assert!(
            !store
                .end_bench_run(id, BenchRunState::Cancelled, Utc::now())
                .unwrap(),
            "a settled run does not move again"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_claim_is_idempotent_capped_and_numbers_attempts() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        store.insert_bench_run(&run(id), Utc::now()).unwrap();

        let BenchClaim::Claimed(first) = store
            .claim_bench_task(id, &claim("t1", "fix-git"), Utc::now())
            .unwrap()
        else {
            panic!("first claim");
        };
        assert_eq!((first.attempt, first.state), (1, BenchTaskState::Starting));
        let BenchClaim::Existing(again) = store
            .claim_bench_task(id, &claim("t1", "fix-git"), Utc::now())
            .unwrap()
        else {
            panic!("repeat claim answers the existing row");
        };
        assert_eq!(again.trial, "t1");

        let BenchClaim::Claimed(second) = store
            .claim_bench_task(id, &claim("t2", "fix-git"), Utc::now())
            .unwrap()
        else {
            panic!("second trial of the task");
        };
        assert_eq!(second.attempt, 2);
        assert_eq!(
            store
                .claim_bench_task(id, &claim("t3", "regex-log"), Utc::now())
                .unwrap(),
            BenchClaim::ConcurrencyFull,
            "two live trials fill a concurrency of two"
        );
        // A settled trial frees its slot.
        store
            .settle_bench_task(
                id,
                "t1",
                &BenchSettlement {
                    state: BenchTaskState::Finished,
                    stop_reason: None,
                    cost_usd: Some(0.01),
                    prompt_tokens: 1,
                    cached_tokens: 0,
                    output_tokens: 1,
                    reasoning_tokens: 0,
                    wall_ms: 5,
                    harness_build_digest: None,
                    trace_sha256: None,
                    route_verdict: Some("valid".into()),
                    route_violations: vec![],
                    finished_at: Utc::now(),
                },
            )
            .unwrap();
        assert!(matches!(
            store
                .claim_bench_task(id, &claim("t3", "regex-log"), Utc::now())
                .unwrap(),
            BenchClaim::Claimed(_)
        ));
        assert_eq!(
            store
                .claim_bench_task(Uuid::new_v4(), &claim("t9", "fix-git"), Utc::now())
                .unwrap(),
            BenchClaim::RunNotActive
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn rows_only_move_forward_and_are_never_deleted() {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        store.insert_bench_run(&run(id), Utc::now()).unwrap();
        let session = seed_session(&store);
        assert!(matches!(
            store
                .claim_bench_task(id, &claim("t1", "fix-git"), Utc::now())
                .unwrap(),
            BenchClaim::Claimed(_)
        ));
        assert!(
            store
                .bind_bench_task_session(id, "t1", session, "/tmp/trace.jsonl")
                .unwrap()
        );
        assert_eq!(
            store
                .get_bench_task_by_session(session)
                .unwrap()
                .unwrap()
                .state,
            BenchTaskState::Running
        );
        let interrupted = store.interrupt_live_bench_tasks(Utc::now()).unwrap();
        assert_eq!(interrupted, [(id, "t1".to_string(), Some(session))]);
        let row = store.get_bench_task(id, "t1").unwrap().unwrap();
        assert_eq!(row.state, BenchTaskState::InfraInterrupted);
        assert_eq!(row.stop_reason.as_deref(), Some("infra_interrupted"));
        // A settled state does not regress, and a second restart finds nothing live.
        assert!(
            store
                .interrupt_live_bench_tasks(Utc::now())
                .unwrap()
                .is_empty()
        );
        let regress = store.conn.execute(
            "UPDATE bench_tasks SET state = 'running' WHERE trial = 't1'",
            [],
        );
        assert!(regress.is_err(), "a settled trial cannot go live again");
        assert!(
            store
                .conn
                .execute("DELETE FROM bench_tasks WHERE trial = 't1'", [])
                .is_err()
        );
        assert!(store.conn.execute("DELETE FROM bench_runs", []).is_err());
        // A cancelled trial's later settlement keeps the state but records usage.
        let late = BenchSettlement {
            state: BenchTaskState::Finished,
            stop_reason: Some("late".into()),
            cost_usd: Some(0.02),
            prompt_tokens: 3,
            cached_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: 0,
            wall_ms: 9,
            harness_build_digest: Some("digest".into()),
            trace_sha256: Some(digest('e')),
            route_verdict: Some("valid".into()),
            route_violations: vec![],
            finished_at: Utc::now(),
        };
        assert!(store.settle_bench_task(id, "t1", &late).unwrap());
        let row = store.get_bench_task(id, "t1").unwrap().unwrap();
        assert_eq!(row.state, BenchTaskState::InfraInterrupted);
        assert_eq!(row.stop_reason.as_deref(), Some("infra_interrupted"));
        assert_eq!(row.cost_usd, Some(0.02));
        assert_eq!(row.trace_sha256, Some(digest('e')));
        let mut other = late.clone();
        other.trace_sha256 = Some(digest('f'));
        assert!(
            store.settle_bench_task(id, "t1", &other).unwrap(),
            "the update succeeds but the first trace hash is kept"
        );
        assert_eq!(
            store
                .get_bench_task(id, "t1")
                .unwrap()
                .unwrap()
                .trace_sha256,
            Some(digest('e'))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn usage_comes_from_the_increment_sql_not_cumulative_rows() {
        let store = Store::open_in_memory().unwrap();
        let session = seed_session(&store);
        // Two session-cumulative rows (running totals 0.10 then 0.25) and one
        // harness turn that owns its cost 0.05: the spend is 0.25 + 0.05, not
        // 0.10 + 0.25 + 0.05.
        for (n, purpose, cost, at) in [
            (1, "session.launch.fresh", 0.10, "2026-10-19T00:00:01Z"),
            (2, "session.continue.resume", 0.25, "2026-10-19T00:00:02Z"),
            (3, "session.harness.turn", 0.05, "2026-10-19T00:00:03Z"),
        ] {
            store
                .insert_invocation_purpose_for_test(session, n, purpose)
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET estimated_cost_usd = ?1, created_at = ?2, \
                     input_tokens = 100, cache_read_tokens = 40, output_tokens = 10, \
                     reasoning_tokens = 2 WHERE session_id = ?3 AND purpose = ?4",
                    params![cost, at, session.to_string(), purpose],
                )
                .unwrap();
        }
        let usage = store.bench_session_usage(session).unwrap();
        assert_eq!(usage.calls, 3);
        let cost = usage.cost_usd.unwrap();
        assert!((cost - 0.30).abs() < 1e-9, "increments, not {cost}");
        assert!((usage.max_call_usd - 0.15).abs() < 1e-9);
        assert_eq!(usage.unpriced_calls, 0);
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.cached_tokens,
                usage.output_tokens
            ),
            (300, 120, 30)
        );
        // An unpriced completed call is counted, not guessed.
        store
            .insert_invocation_purpose_for_test(session, 4, "session.harness.turn")
            .unwrap();
        let usage = store.bench_session_usage(session).unwrap();
        assert_eq!(usage.unpriced_calls, 1);
        assert_eq!(
            store.bench_session_usage(Uuid::new_v4()).unwrap(),
            BenchSessionUsage::default()
        );
    }
}
