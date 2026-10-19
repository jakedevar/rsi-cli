//! Durable daemon-owned agent jobs (#1002 slice 1).
//!
//! One row per submitted job. The job itself runs in a `systemd-run --user`
//! unit outside every session scope; this table is the typed record. Settlement
//! CASes `running -> terminal` and, in the same transaction, inserts the single
//! owner wake (`scheduled_jobs`, mode `resume`), so a second settle (a poll
//! racing a restart reconcile) writes nothing.

use super::Store;
use super::scheduled_jobs::insert_scheduled_job_conn;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::agent_jobs::{
    AgentJobResultV1, AgentJobV1, JOB_HELD_FOR_DEPLOY, JOB_KEY_CONFLICT, JOB_QA_LANE_LIMIT,
    JOB_QA_LANE_MAX_RUNNING, JobKind, JobParams, JobState, JobWake,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params};
use uuid::Uuid;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: agent-jobs-migration
/// Provisional schema version of the agent job catalog; the lander renumbers it.
pub(crate) const AGENT_JOBS_SCHEMA_VERSION: i32 = 144;

const CATALOG: &str = "CREATE TABLE agent_jobs (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(id)),
    owner_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(owner_session_id)),
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    kind TEXT NOT NULL CHECK(kind IN ('test','build','landing','cloud_gate')),
    name TEXT CHECK(name IS NULL OR length(name)>0),
    params_json TEXT NOT NULL CHECK(json_valid(params_json)),
    cwd TEXT NOT NULL CHECK(length(cwd)>0),
    unit_name TEXT NOT NULL UNIQUE CHECK(length(unit_name)>0),
    log_path TEXT NOT NULL CHECK(length(log_path)>0),
    status_path TEXT NOT NULL CHECK(length(status_path)>0),
    state TEXT NOT NULL CHECK(state IN ('running','succeeded','failed','lost')),
    exit_code INTEGER,
    result_json TEXT CHECK(result_json IS NULL OR json_valid(result_json)),
    idempotency_key TEXT CHECK(idempotency_key IS NULL OR length(idempotency_key)>0),
    wake_job_id TEXT UNIQUE CHECK(wake_job_id IS NULL OR rsi_uuid_is_canonical(wake_job_id)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    finished_at TEXT CHECK(finished_at IS NULL OR rsi_rfc3339_nanos_is_canonical(finished_at)),
    row_version INTEGER NOT NULL CHECK(row_version>0)
);
CREATE UNIQUE INDEX agent_jobs_owner_key ON agent_jobs(owner_session_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX agent_jobs_by_state ON agent_jobs(state, sequence);
CREATE INDEX agent_jobs_by_owner ON agent_jobs(owner_session_id, sequence);
CREATE TRIGGER agent_jobs_no_delete BEFORE DELETE ON agent_jobs BEGIN SELECT RAISE(ABORT,'agent jobs are append only'); END;
CREATE TRIGGER agent_jobs_terminal_immutable BEFORE UPDATE ON agent_jobs WHEN OLD.state IN ('succeeded','failed','lost') BEGIN SELECT RAISE(ABORT,'terminal agent jobs are immutable'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 6] = [
    ("table", "agent_jobs"),
    ("index", "agent_jobs_owner_key"),
    ("index", "agent_jobs_by_state"),
    ("index", "agent_jobs_by_owner"),
    ("trigger", "agent_jobs_no_delete"),
    ("trigger", "agent_jobs_terminal_immutable"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "agent jobs require V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: agent-jobs-migration

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Hand live jobs and rolling-queue outcomes to a proven successor of the same manager seat. Callers
/// validate seat custody and hold the publication transaction; ordinary scope
/// edits and uncommitted continuation rows never call this helper. Seat
/// transitions call `manager_obligations::transfer_manager_obligations_on`,
/// which runs this and then carries the rest of the seat's durable result
/// and predicate obligations (#2471).
pub(crate) fn transfer_manager_jobs_on(
    conn: &rusqlite::Connection,
    predecessor: Uuid,
    successor: Uuid,
) -> Result<usize> {
    if conn.is_autocommit() {
        return Err(DaemonError::Store(
            "manager job transfer requires a transaction".into(),
        ));
    }
    if predecessor == successor {
        return Ok(0);
    }
    let old = predecessor.to_string();
    let new = successor.to_string();
    // Replay identity is (owner, key). Refuse the whole seat publication if
    // transferring a live job would make a successor's key ambiguous.
    let conflict: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_jobs live
         JOIN agent_jobs prior ON prior.owner_session_id=?2
           AND prior.idempotency_key=live.idempotency_key
         WHERE live.owner_session_id=?1 AND live.state IN ('queued','running'))",
        params![old, new],
        |row| row.get(0),
    )?;
    if conflict {
        return Err(DaemonError::InvalidParam(
            "manager_job_transfer_key_conflict".into(),
        ));
    }
    // Queue outcomes follow the same proven project/portfolio succession,
    // even when the outgoing seat owns no live daemon job.
    super::rolling_queue::transfer_manager_queue_wakes_on(conn, predecessor, successor)?;
    let moved = conn.execute(
        "UPDATE agent_jobs SET owner_session_id=?2,row_version=row_version+1
         WHERE owner_session_id=?1 AND state IN ('queued','running')",
        params![old, new],
    )?;
    // Batch waits, predicate waits and outcome wakes follow in
    // `manager_obligations::transfer_manager_obligations_on` (#2471).
    Ok(moved)
}

/// Everything the submit verb resolves before the store write.
#[derive(Debug, Clone)]
pub struct NewAgentJob {
    pub id: Uuid,
    pub owner_session_id: Uuid,
    pub project_id: Option<Uuid>,
    pub name: Option<String>,
    pub params: JobParams,
    pub cwd: String,
    pub unit_name: String,
    pub log_path: String,
    pub status_path: String,
    pub idempotency_key: Option<String>,
    /// Per-job completion wake policy (#1006).
    pub wake: JobWake,
}

/// One job plus the daemon-private status file path.
#[derive(Debug, Clone)]
pub struct AgentJobRow {
    pub job: AgentJobV1,
    pub status_path: String,
}

/// #2278: the `params_json` key that records the deploy which stopped a running
/// test job and put it back in the queue (decoded away like `wake` and `held`).
pub(crate) const REQUEUED_BY_DEPLOY_KEY: &str = "requeued_by_deploy";

const COLUMNS: &str = "id, owner_session_id, kind, name, params_json, cwd, unit_name, log_path, \
    status_path, state, exit_code, result_json, created_at, finished_at, started_at";

fn conversion(
    index: usize,
    message: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, message.into())
}

fn parse_uuid(index: usize, value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| conversion(index, error))
}

/// The wake policy rides in `params_json` as a `wake` key beside the typed
/// params (no column): absent means `owner`, the default. It is stripped
/// before the typed params are decoded. The daemon-selected queue reason
/// uses a `held` key; older queued rows default to the deploy reason
/// (`held_for_deploy`, #2190).
fn decode_params(text: &str) -> serde_json::Result<(JobParams, JobWake, Option<String>)> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    let held = value
        .as_object_mut()
        .and_then(|object| object.remove("held"))
        .map(serde_json::from_value)
        .transpose()?;
    // #2278: the deploy that stopped and requeued this job; a record, not a param.
    if let Some(object) = value.as_object_mut() {
        object.remove(REQUEUED_BY_DEPLOY_KEY);
    }
    let wake = match value
        .as_object_mut()
        .and_then(|object| object.remove("wake"))
    {
        Some(wake) => serde_json::from_value(wake)?,
        None => JobWake::Owner,
    };
    Ok((serde_json::from_value(value)?, wake, held))
}

fn encode_params(
    params: &JobParams,
    wake: JobWake,
    held: Option<&str>,
) -> serde_json::Result<String> {
    let mut value = serde_json::to_value(params)?;
    if let Some(held) = held
        && let Some(object) = value.as_object_mut()
    {
        object.insert("held".into(), serde_json::json!(held));
    }
    if wake != JobWake::Owner
        && let Some(object) = value.as_object_mut()
    {
        object.insert("wake".into(), serde_json::json!(wake));
    }
    serde_json::to_string(&value)
}

fn map_row(row: &Row<'_>) -> rusqlite::Result<AgentJobRow> {
    let kind_text: String = row.get(2)?;
    let kind = JobKind::parse(&kind_text)
        .ok_or_else(|| conversion(2, format!("unknown job kind {kind_text}")))?;
    let state_text: String = row.get(9)?;
    let state = JobState::parse(&state_text)
        .ok_or_else(|| conversion(9, format!("unknown job state {state_text}")))?;
    let (params, wake, held) =
        decode_params(&row.get::<_, String>(4)?).map_err(|error| conversion(4, error))?;
    let result = row
        .get::<_, Option<String>>(11)?
        .map(|text| serde_json::from_str::<AgentJobResultV1>(&text))
        .transpose()
        .map_err(|error| conversion(11, error))?;
    Ok(AgentJobRow {
        status_path: row.get(8)?,
        job: AgentJobV1 {
            id: parse_uuid(0, &row.get::<_, String>(0)?)?,
            owner_session_id: parse_uuid(1, &row.get::<_, String>(1)?)?,
            kind,
            name: row.get(3)?,
            params,
            cwd: row.get(5)?,
            unit_name: row.get(6)?,
            log_path: row.get(7)?,
            state,
            exit_code: row.get(10)?,
            result,
            created_at: row.get(12)?,
            started_at: row.get(14)?,
            finished_at: row.get(13)?,
            held: (state == JobState::Queued)
                .then(|| held.unwrap_or_else(|| JOB_HELD_FOR_DEPLOY.to_string())),
            wake,
        },
    })
}

/// Human/agent-readable wake body for one terminal job.
pub fn wake_message(job: &AgentJobV1, state: JobState, result: &AgentJobResultV1) -> String {
    let mut text = format!(
        "[job] {} job {}{}: {}",
        job.kind.as_str(),
        job.id,
        job.name
            .as_deref()
            .map_or_else(String::new, |name| format!(" ({name})")),
        state.as_str()
    );
    if let Some(code) = result.exit_code {
        text.push_str(&format!("; exit_code={code}"));
    }
    if let Some(sha) = &result.landed_sha {
        text.push_str(&format!("; landed_sha={sha}"));
    }
    if let Some(refusal) = &result.refusal {
        text.push_str(&format!("; refusal={refusal}"));
    }
    if !result.failing_tests.is_empty() {
        text.push_str(&format!(
            "; failing_tests={}",
            result.failing_tests.join(",")
        ));
    }
    if result.failing_tests_omitted > 0 {
        text.push_str(&format!(
            "; failing_tests_omitted={}",
            result.failing_tests_omitted
        ));
    }
    if let Some(sweep) = &result.sweep {
        text.push_str(&format!(
            "; verdict={}; sha={}; results_dir={}",
            sweep.verdict.as_str(),
            sweep.sha,
            sweep.results_dir
        ));
        if !sweep.new_failures.is_empty() {
            text.push_str(&format!("; new_failures={}", sweep.new_failures.join(",")));
        }
        if !sweep.known_failures.is_empty() {
            text.push_str(&format!(
                "; known_failures={}",
                sweep.known_failures.join(",")
            ));
        }
    }
    if let Some(receipt) = &result.receipt {
        // #1099: the wake carries the typed receipt, minus the per-file shard
        // map (the full receipt stays in the job result).
        let mut compact = receipt.clone();
        if let Some(shards) = compact.get_mut("shards").and_then(|v| v.as_object_mut()) {
            shards.remove("map");
        }
        text.push_str(&format!("; receipt={compact}"));
    }
    text.push_str(&format!("; log={}", job.log_path));
    if let Some(detail) = &result.detail {
        text.push_str(&format!("; detail={detail}"));
    }
    text
}

impl Store {
    /// #1520: read an explicitly delegated, live Issue-launch binding for QA admission.
    /// Job params, titles, Issue labels and rotation lineage grant nothing.
    pub fn live_qa_lane_binding(&self, worker: Uuid) -> Result<Option<(Uuid, Uuid, Uuid)>> {
        let Some(binding @ (_, _, operation)) =
            Self::live_issue_binding_for_worker_on(&self.conn, worker)?
        else {
            return Ok(None);
        };
        Ok(self
            .manager_action_issue_binding(operation)?
            .filter(|grant| grant.qa_lane)
            .map(|_| binding))
    }

    /// Record a new `running` job. A replay of the same `(owner, key)` with the
    /// same kind, params and name returns the original job and `replayed=true`;
    /// a different request under the same key is refused.
    pub fn insert_agent_job(
        &self,
        new: &NewAgentJob,
        now: DateTime<Utc>,
    ) -> Result<(AgentJobRow, bool)> {
        self.insert_agent_job_in_state(new, now, JobState::Running)
    }

    /// [`Self::insert_agent_job`] with the initial state: `Running` (the unit
    /// is launched next) or `Queued` (held for a deploy, #1566/#2190; no unit
    /// until [`Self::start_queued_agent_job`]).
    pub fn insert_agent_job_in_state(
        &self,
        new: &NewAgentJob,
        now: DateTime<Utc>,
        state: JobState,
    ) -> Result<(AgentJobRow, bool)> {
        self.insert_agent_job_with_hold_reason(new, now, state, None)
    }

    /// Store a daemon-selected queue reason beside the wake policy, without
    /// changing the typed request or its idempotency comparison.
    pub fn insert_agent_job_with_hold_reason(
        &self,
        new: &NewAgentJob,
        now: DateTime<Utc>,
        state: JobState,
        held: Option<&str>,
    ) -> Result<(AgentJobRow, bool)> {
        debug_assert!(matches!(state, JobState::Queued | JobState::Running));
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let params_json = encode_params(&new.params, new.wake, held)
            .map_err(|e| DaemonError::Store(e.to_string()))?;
        if let Some(key) = &new.idempotency_key {
            let existing = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM agent_jobs \
                         WHERE owner_session_id=?1 AND idempotency_key=?2"
                    ),
                    params![new.owner_session_id.to_string(), key],
                    map_row,
                )
                .optional()?;
            if let Some(row) = existing {
                let same = row.job.params == new.params
                    && row.job.name == new.name
                    && row.job.wake == new.wake;
                return if same {
                    Ok((row, true))
                } else {
                    Err(DaemonError::InvalidParam(JOB_KEY_CONFLICT.into()))
                };
            }
        }
        // #1520: serialize QA capacity with the insertion, after replay so
        // retries at capacity remain harmless. Ordinary jobs are unchanged.
        if matches!(&new.params, JobParams::Test(test) if test.qa_lane.is_some()) {
            let outstanding: u32 = tx.query_row(
                "SELECT count(*) FROM agent_jobs WHERE owner_session_id=?1
                 AND state IN ('queued','running') AND kind='test'
                 AND json_type(params_json,'$.qa_lane')='object'",
                [new.owner_session_id.to_string()],
                |row| row.get(0),
            )?;
            if outstanding >= JOB_QA_LANE_MAX_RUNNING {
                return Err(DaemonError::PolicyDenied(JOB_QA_LANE_LIMIT.into()));
            }
        }
        // Serialize job admission against target reclaim's Prepared gate.
        // The reclaimer checks running jobs in its writer transaction; a
        // later job waits until the old cache is detached or prepare abandoned.
        let reclaim_prepared: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_target_reclaim_intents i
             JOIN sandbox_custody_roots r ON r.custody_id=i.custody_id AND r.generation=i.generation
             WHERE i.state='Prepared'
               AND (r.owner_session_id=?1 OR r.sandbox_root=?2
                    OR substr(?2,1,length(r.sandbox_root)+1)=r.sandbox_root || '/'))",
            params![new.owner_session_id.to_string(), new.cwd],
            |row| row.get(0),
        )?;
        if reclaim_prepared {
            return Err(DaemonError::InvalidParam("sandbox_reclaim_prepared".into()));
        }
        tx.execute(
            "INSERT INTO agent_jobs(id, owner_session_id, project_id, kind, name, params_json, \
             cwd, unit_name, log_path, status_path, state, idempotency_key, created_at, \
             started_at, row_version) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?13,?11,?12,\
             CASE ?13 WHEN 'running' THEN ?12 END,1)",
            params![
                new.id.to_string(),
                new.owner_session_id.to_string(),
                new.project_id.map(|id| id.to_string()),
                new.params.kind().as_str(),
                new.name,
                params_json,
                new.cwd,
                new.unit_name,
                new.log_path,
                new.status_path,
                new.idempotency_key,
                stamp(now),
                state.as_str(),
            ],
        )?;
        let row = tx.query_row(
            &format!("SELECT {COLUMNS} FROM agent_jobs WHERE id=?1"),
            [new.id.to_string()],
            map_row,
        )?;
        tx.commit()?;
        Ok((row, false))
    }

    pub fn get_agent_job(&self, id: Uuid) -> Result<Option<AgentJobRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agent_jobs WHERE id=?1"),
                [id.to_string()],
                map_row,
            )
            .optional()?)
    }

    /// The project a job was recorded under (`None` for a legacy row or a
    /// job submitted outside any project), or `None` when the job is absent.
    pub fn agent_job_project(&self, id: Uuid) -> Result<Option<Uuid>> {
        let project: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT project_id FROM agent_jobs WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(project
            .flatten()
            .and_then(|project| Uuid::parse_str(&project).ok()))
    }

    /// The newest job recorded under `key`, whichever session owns it. A
    /// satellite finds a hub QA run (#2120) by its run key even after the
    /// owning seat rotated; callers use keys that embed a unique run id.
    pub fn find_agent_job_by_key(&self, key: &str) -> Result<Option<AgentJobRow>> {
        Ok(self
            .conn
            .query_row(
                // sql-dynamic-ok: static column list; the key is a bound parameter
                &format!(
                    "SELECT {COLUMNS} FROM agent_jobs WHERE idempotency_key=?1 \
                     ORDER BY sequence DESC LIMIT 1"
                ),
                [key],
                map_row,
            )
            .optional()?)
    }

    /// Newest first, scoped to one owner.
    pub fn list_agent_jobs(&self, owner: Uuid, limit: usize) -> Result<Vec<AgentJobRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM agent_jobs WHERE owner_session_id=?1 \
             ORDER BY sequence DESC LIMIT ?2"
        ))?;
        let rows = statement
            .query_map(params![owner.to_string(), limit as i64], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// #2470: whose jobs `caller` may read besides its own, because it holds
    /// the current manager seat. Returns the seat's published rotation
    /// predecessors (never `caller`) and, for a project seat, the project the
    /// job rows must also name. `None` for every caller that is not the live
    /// seat of a project, area node or portfolio node: a stale predecessor, a
    /// worker, a foreign or revoked manager. Read authority only; the job
    /// rows, their owner and the cancel rule are untouched.
    pub fn manager_job_seat_reach(
        &self,
        caller: Uuid,
    ) -> Result<Option<(Vec<Uuid>, Option<Uuid>)>> {
        let portfolio_node = if self.portfolio_seat_grant(caller)?.is_some() {
            self.portfolio_seat_node(caller)?
        } else {
            None
        };
        if let Some(node) = portfolio_node {
            let mut owners = Vec::new();
            let mut cursor = self.get_session(caller)?.and_then(|row| row.continued_from);
            for _ in 0..super::satellite_dispatch::SATELLITE_LINEAGE_MAX_HOPS {
                let Some(member) = cursor else { break };
                if member == caller
                    || owners.contains(&member)
                    || self.portfolio_seat_node(member)? != Some(node)
                {
                    break;
                }
                owners.push(member);
                cursor = self.get_session(member)?.and_then(|row| row.continued_from);
            }
            return Ok(Some((owners, None)));
        }
        let Some(project) = self.get_session(caller)?.and_then(|row| row.project_id) else {
            return Ok(None);
        };
        let project_seat = self.get_harness_manager(project)?.is_some_and(|config| {
            config.current_session_id == Some(caller) && !config.is_revoked()
        });
        if !project_seat && self.manager_area_authority_current(caller)?.is_none() {
            return Ok(None);
        }
        let Some(lineage) = self.project_seat_lineage(project, caller)? else {
            return Ok(None);
        };
        let owners = lineage.into_iter().filter(|id| *id != caller).collect();
        Ok(Some((owners, Some(project))))
    }

    /// #132: whether job `id` belongs to a published rotation predecessor of
    /// the live manager seat `caller` in the seat's own project (or portfolio
    /// node). The one observer rule behind `get_agent_job_for_reader` and the
    /// `jobs_terminal` watch authorization, so read, list and watch agree.
    /// Observation only: the job row and its owner are never changed, and no
    /// control (cancel) authority follows from it.
    pub(crate) fn seat_predecessor_job_observable(&self, caller: Uuid, id: Uuid) -> Result<bool> {
        let job: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT owner_session_id, project_id FROM agent_jobs WHERE id=?1",
                [id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((owner, job_project)) = job else {
            return Ok(false);
        };
        let Ok(owner) = Uuid::parse_str(&owner) else {
            return Ok(false);
        };
        let Some((predecessors, project)) = self.manager_job_seat_reach(caller)? else {
            return Ok(false);
        };
        let project_matches = project.is_none_or(|p| job_project == Some(p.to_string()));
        Ok(project_matches && predecessors.contains(&owner))
    }

    /// #132: whether `owner` may name job `id` in a `jobs_terminal` watch: its
    /// own job in any state, or a job that already settled under a published
    /// rotation predecessor of its live seat (a job that finished before the
    /// successor was published keeps its original owner and receipt). A live
    /// predecessor job is not watchable here: succession already moved it.
    pub(crate) fn job_watchable_by(&self, owner: Uuid, id: Uuid) -> Result<bool> {
        let job: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT owner_session_id, state FROM agent_jobs WHERE id=?1",
                [id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((job_owner, state)) = job else {
            return Ok(false);
        };
        if job_owner == owner.to_string() {
            return Ok(true);
        }
        if matches!(state.as_str(), "queued" | "running") {
            return Ok(false);
        }
        self.seat_predecessor_job_observable(owner, id)
    }

    /// #2470: `get_agent_job` for a reader. The owner always reads its own
    /// job; the current manager seat also reads (a) a job a published rotation
    /// predecessor of the seat owned in this project (or, for a portfolio
    /// seat, under the same node), settled or not, and (b) a job owned by a
    /// session inside its live manager scope, such as a worker's receipt.
    pub fn get_agent_job_for_reader(&self, caller: Uuid, id: Uuid) -> Result<Option<AgentJobRow>> {
        let Some(row) = self.get_agent_job(id)? else {
            return Ok(None);
        };
        let owner = row.job.owner_session_id;
        if owner == caller {
            return Ok(Some(row));
        }
        if self.seat_predecessor_job_observable(caller, id)? {
            return Ok(Some(row));
        }
        if self
            .manager_session_control_scope(caller, owner, false)?
            .is_some()
        {
            return Ok(Some(row));
        }
        Ok(None)
    }

    /// #2470: `list_agent_jobs` for a reader: the caller's own jobs plus those
    /// of its seat's published rotation predecessors, newest first.
    pub fn list_agent_jobs_for_reader(
        &self,
        caller: Uuid,
        limit: usize,
    ) -> Result<Vec<AgentJobRow>> {
        let Some((owners, project)) = self.manager_job_seat_reach(caller)? else {
            return self.list_agent_jobs(caller, limit);
        };
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM agent_jobs WHERE owner_session_id=?1 \
             OR (owner_session_id IN (SELECT value FROM json_each(?2)) \
                 AND (?3 IS NULL OR project_id=?3)) \
             ORDER BY sequence DESC LIMIT ?4"
        ))?;
        let owners = serde_json::to_string(&owners)
            .map_err(|error| DaemonError::Store(format!("job reader owners: {error}")))?;
        let rows = statement
            .query_map(
                params![
                    caller.to_string(),
                    owners,
                    project.map(|id| id.to_string()),
                    limit as i64
                ],
                map_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// #3038: the jobs `owner` submitted that are still `queued` or `running`,
    /// oldest first.
    pub fn list_open_agent_jobs_for_owner(&self, owner: Uuid) -> Result<Vec<AgentJobRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM agent_jobs WHERE owner_session_id=?1 \
             AND state IN ('queued','running') ORDER BY sequence"
        ))?;
        let rows = statement
            .query_map(params![owner.to_string()], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every job still `running`, oldest first (the runner's poll set).
    pub fn list_running_agent_jobs(&self) -> Result<Vec<AgentJobRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM agent_jobs WHERE state='running' ORDER BY sequence"
        ))?;
        let rows = statement
            .query_map([], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every job still `queued` behind a deploy drain, oldest first.
    pub fn list_queued_agent_jobs(&self) -> Result<Vec<AgentJobRow>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM agent_jobs WHERE state='queued' ORDER BY sequence"
        ))?;
        let rows = statement
            .query_map([], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Whether `owner` has a job that is still `queued` or `running` and was
    /// submitted with the default `owner` wake policy (#2262). Its settlement
    /// inserts the owner's resume wake, so the session's turn may end now and
    /// still continue; the wake row does not exist until then. A `wake: none`
    /// job never resumes the session and does not count.
    pub fn session_has_live_owner_wake_job(&self, owner: Uuid) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_jobs WHERE owner_session_id=?1 \
                 AND state IN ('queued','running') \
                 AND COALESCE(json_extract(params_json,'$.wake'),'owner')='owner')",
                [owner.to_string()],
                |row| row.get(0),
            )
            .map_err(DaemonError::Database)
    }

    /// #2476: the session's wake for a job that just settled is in flight. A
    /// `when.jobs_terminal` wake (or the owner wake a settlement inserts) is
    /// consumed when it fires, and the session stays `Completed` until the
    /// resume turn starts, so for that window it has neither an armed wake nor a
    /// live job although the daemon is about to resume it. True when `owner` has
    /// a job that reached a terminal state after the session's last assistant
    /// message (the turn that parked on it) and within `grace` of `now`. The
    /// grace bounds a resume that never arrives; with no assistant message
    /// recorded nothing can be compared and the answer is false.
    pub fn session_job_settled_since_last_turn(
        &self,
        owner: Uuid,
        now: DateTime<Utc>,
        grace: chrono::Duration,
    ) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_jobs j WHERE j.owner_session_id=?1 \
                 AND j.state NOT IN ('queued','running') AND j.finished_at IS NOT NULL \
                 AND julianday(j.finished_at) >= julianday(?2) \
                 AND julianday(j.finished_at) > (SELECT julianday(MAX(e.created_at)) \
                      FROM conversation_events e WHERE e.session_id=?1 \
                      AND e.event_type='Message' AND e.role='Assistant'))",
                params![owner.to_string(), stamp(now - grace)],
                |row| row.get(0),
            )
            .map_err(DaemonError::Database)
    }

    /// CAS `queued -> running` and stamp `started_at`. `false` when the job
    /// was no longer queued (cancelled, or another poll started it first).
    pub fn start_queued_agent_job(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE agent_jobs SET state='running', started_at=?2, \
             row_version=row_version+1 WHERE id=?1 AND state='queued'",
            params![id.to_string(), stamp(now)],
        )?;
        Ok(changed == 1)
    }

    /// #2278: CAS `running -> queued` for a job a staged `interrupt_workers`
    /// deploy stopped. The row keeps its id, owner, params and wake policy, so
    /// the owner's single completion wake (or its `jobs_terminal` wait) fires when
    /// the rerun ends, not when the deploy stopped it. The deploy that did it is
    /// recorded under `requeued_by_deploy` and the job reads `held_for_deploy`
    /// until the hold ends. False when the job was not `running`.
    pub fn requeue_agent_job_for_deploy(&self, id: Uuid, deploy: Uuid) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE agent_jobs SET state='queued', started_at=NULL, exit_code=NULL, \
             result_json=NULL, finished_at=NULL, wake_job_id=NULL, \
             params_json=json_set(params_json,'$.held',?3,'$.requeued_by_deploy',?2), \
             row_version=row_version+1 WHERE id=?1 AND state='running'",
            params![id.to_string(), deploy.to_string(), JOB_HELD_FOR_DEPLOY],
        )? == 1)
    }

    /// #2278/#2204: the deploy that stopped and requeued job `id`, if any. The
    /// record survives the rerun, so the owner's wake can say that the receipt
    /// it carries is the rerun's, not the interrupted run's.
    pub fn agent_job_requeued_by_deploy(&self, id: Uuid) -> Result<Option<Uuid>> {
        let deploy: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(params_json,'$.requeued_by_deploy') \
                 FROM agent_jobs WHERE id=?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(deploy.and_then(|text| Uuid::parse_str(&text).ok()))
    }

    /// CAS `running -> terminal`. With `wake`, insert the owner's single resume
    /// wake in the same transaction. Returns the wake id when one was written;
    /// `None` when the job was not `running` (already settled: nothing is
    /// written) or `wake` was false.
    pub fn settle_agent_job(
        &self,
        id: Uuid,
        terminal: JobState,
        result: &AgentJobResultV1,
        wake: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>> {
        Ok(self
            .settle_agent_job_outcome(id, terminal, result, wake, now)?
            .flatten())
    }

    /// [`Self::settle_agent_job`] that also reports a silent settlement:
    /// `None` when the job was not `running`, `Some(None)` when it settled
    /// without a wake (`wake` false, #1006), `Some(Some(id))` with the wake.
    pub fn settle_agent_job_outcome(
        &self,
        id: Uuid,
        terminal: JobState,
        result: &AgentJobResultV1,
        wake: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<Option<Uuid>>> {
        if !terminal.is_terminal() {
            return Err(DaemonError::InvalidParam(
                "job_settle_state_not_terminal".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(row) = tx
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM agent_jobs WHERE id=?1 AND state IN ('queued','running')"
                ),
                [id.to_string()],
                map_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let wake_id = wake.then(Uuid::new_v4);
        let result_json =
            serde_json::to_string(result).map_err(|e| DaemonError::Store(e.to_string()))?;
        let changed = tx.execute(
            "UPDATE agent_jobs SET state=?2, exit_code=?3, result_json=?4, wake_job_id=?5, \
             finished_at=?6, row_version=row_version+1 \
             WHERE id=?1 AND state IN ('queued','running')",
            params![
                id.to_string(),
                terminal.as_str(),
                result.exit_code,
                result_json,
                wake_id.map(|id| id.to_string()),
                stamp(now)
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        super::friction::note_agent_job_friction_in(&tx, &row.job, terminal, now);
        if let Some(wake_id) = wake_id {
            let job = row.job;
            let message = wake_message(&job, terminal, result);
            let scheduled = ScheduledJob {
                id: wake_id,
                name: format!("job-{}", job.id),
                message,
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
                wake_session_id: Some(job.owner_session_id),
            };
            insert_scheduled_job_conn(&tx, &scheduled)?;
        }
        tx.commit()?;
        Ok(Some(wake_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::{BuildCommand, BuildJobParams};

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn new_job(owner: Uuid, key: Option<&str>) -> NewAgentJob {
        let id = Uuid::new_v4();
        NewAgentJob {
            id,
            owner_session_id: owner,
            project_id: None,
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
            idempotency_key: key.map(str::to_string),
            wake: JobWake::Owner,
        }
    }

    fn done() -> AgentJobResultV1 {
        AgentJobResultV1 {
            exit_code: Some(0),
            ..AgentJobResultV1::default()
        }
    }

    /// #2278: a running job a deploy stopped goes back to `queued` under its own
    /// id, owner, params and wake policy, held for the deploy, with no outcome
    /// and no wake; the deploy that did it is recorded and the job can run again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_job_requeued_for_a_deploy_keeps_its_identity_and_names_the_deploy() {
        let store = store();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let mut new = new_job(owner, Some("k"));
        new.wake = JobWake::None;
        let (row, _) = store.insert_agent_job(&new, now).unwrap();
        let deploy = Uuid::new_v4();
        assert!(
            store
                .requeue_agent_job_for_deploy(row.job.id, deploy)
                .unwrap()
        );

        let queued = store.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(queued.state, JobState::Queued);
        assert_eq!(queued.held.as_deref(), Some(JOB_HELD_FOR_DEPLOY));
        assert_eq!(queued.owner_session_id, owner);
        assert_eq!(queued.params, new.params);
        assert_eq!(queued.wake, JobWake::None);
        assert_eq!(
            store.agent_job_requeued_by_deploy(row.job.id).unwrap(),
            Some(deploy),
            "#2204: the requeue is readable for the owner's wake"
        );
        assert_eq!(
            store.agent_job_requeued_by_deploy(Uuid::new_v4()).unwrap(),
            None
        );
        assert_eq!(queued.started_at, None);
        assert!(queued.result.is_none() && queued.finished_at.is_none());
        assert_eq!(store.deploy_cancelled_jobs(deploy).unwrap(), [row.job.id]);
        assert!(
            store
                .deploy_cancelled_jobs(Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
        // A replay of the original submit still finds the one job.
        let (replayed, was_replay) = store.insert_agent_job(&new, now).unwrap();
        assert!(was_replay);
        assert_eq!(replayed.job.id, row.job.id);
        // Only a running job is requeued; the rerun starts and can end.
        assert!(
            !store
                .requeue_agent_job_for_deploy(row.job.id, deploy)
                .unwrap()
        );
        assert!(store.start_queued_agent_job(row.job.id, now).unwrap());
        assert!(
            store
                .requeue_agent_job_for_deploy(row.job.id, deploy)
                .unwrap()
        );
        assert!(store.start_queued_agent_job(row.job.id, now).unwrap());
        assert!(
            store
                .settle_agent_job(row.job.id, JobState::Succeeded, &done(), false, now)
                .unwrap()
                .is_none()
        );
        let ended = store.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(ended.state, JobState::Succeeded);
        assert_eq!(store.deploy_cancelled_jobs(deploy).unwrap(), [row.job.id]);
        assert_eq!(
            store.agent_job_requeued_by_deploy(row.job.id).unwrap(),
            Some(deploy),
            "the record outlives the rerun"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn qa_lane_admission_is_per_owner_replay_safe_and_releases_capacity() {
        let store = store();
        let owner = Uuid::new_v4();
        let qa = |owner, key| {
            let mut new = new_job(owner, Some(key));
            new.params = rsi_common::agent_jobs::AgentSubmitJobRequestV1 {
                kind: JobKind::Test,
                params: serde_json::json!({"shard":"store-01",
                    "qa_lane":{"sha":"a".repeat(40)},"timeout_minutes":90}),
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: None,
                project_id: None,
                sandbox_session_id: None,
            }
            .typed_params()
            .unwrap();
            new
        };
        let first = qa(owner, "one");
        store.insert_agent_job(&first, Utc::now()).unwrap();
        store
            .insert_agent_job_in_state(&qa(owner, "two"), Utc::now(), JobState::Queued)
            .unwrap();
        assert!(
            store
                .insert_agent_job(&qa(owner, "three"), Utc::now())
                .unwrap_err()
                .to_string()
                .contains("job_qa_lane_limit")
        );
        let (replay, replayed) = store
            .insert_agent_job(&qa(owner, "one"), Utc::now())
            .unwrap();
        assert!(replayed);
        assert_eq!(replay.job.id, first.id);
        store
            .insert_agent_job(&qa(Uuid::new_v4(), "other"), Utc::now())
            .unwrap();
        store
            .insert_agent_job(&new_job(owner, None), Utc::now())
            .unwrap();
        store
            .settle_agent_job(first.id, JobState::Succeeded, &done(), false, Utc::now())
            .unwrap();
        store
            .insert_agent_job(&qa(owner, "three"), Utc::now())
            .unwrap();
        assert_eq!(store.list_running_agent_jobs().unwrap().len(), 3);
        assert_eq!(store.list_queued_agent_jobs().unwrap().len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_job_records_every_field_and_replays_by_key() {
        let store = store();
        let owner = Uuid::new_v4();
        let new = new_job(owner, Some("k1"));
        let (row, replayed) = store.insert_agent_job(&new, Utc::now()).expect("insert");
        assert!(!replayed);
        assert_eq!(row.job.id, new.id);
        assert_eq!(row.job.state, JobState::Running);
        assert_eq!(row.job.kind, JobKind::Build);
        assert_eq!(row.job.log_path, new.log_path);
        assert_eq!(row.status_path, new.status_path);

        let mut again = new_job(owner, Some("k1"));
        again.id = Uuid::new_v4();
        let (replay, replayed) = store.insert_agent_job(&again, Utc::now()).expect("replay");
        assert!(replayed);
        assert_eq!(replay.job.id, new.id);

        let mut different = new_job(owner, Some("k1"));
        different.name = Some("other".into());
        let error = store
            .insert_agent_job(&different, Utc::now())
            .expect_err("conflicting key");
        assert!(error.to_string().contains(JOB_KEY_CONFLICT));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn settling_twice_inserts_exactly_one_owner_wake() {
        let store = store();
        let owner = Uuid::new_v4();
        let new = new_job(owner, None);
        store.insert_agent_job(&new, Utc::now()).expect("insert");
        let now = Utc::now();
        let wake = store
            .settle_agent_job(new.id, JobState::Succeeded, &done(), true, now)
            .expect("settle")
            .expect("first settle wakes");
        let second = store
            .settle_agent_job(new.id, JobState::Failed, &done(), true, now)
            .expect("second settle");
        assert!(second.is_none(), "a settled job must not wake again");
        let wakes: Vec<_> = store
            .list_scheduled_jobs()
            .expect("wakes")
            .into_iter()
            .filter(|job| job.wake_session_id == Some(owner))
            .collect();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].id, wake);
        assert_eq!(wakes[0].wake_mode, WakeMode::Resume);
        assert!(wakes[0].message.contains(&new.id.to_string()));
        assert!(wakes[0].message.contains("succeeded"));
        let job = store.get_agent_job(new.id).expect("get").expect("row").job;
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.exit_code, Some(0));
    }

    /// #2262: only a live job with the default `owner` wake will resume its
    /// owner when it settles; a `wake: none` job, a settled job and another
    /// session's job do not count.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn live_owner_wake_job_is_found_only_for_its_owner_while_unsettled() {
        let store = store();
        let owner = Uuid::new_v4();
        assert!(!store.session_has_live_owner_wake_job(owner).expect("none"));

        let mut silent = new_job(owner, None);
        silent.wake = JobWake::None;
        store.insert_agent_job(&silent, Utc::now()).expect("insert");
        assert!(
            !store
                .session_has_live_owner_wake_job(owner)
                .expect("silent"),
            "a wake:none job never resumes its owner"
        );

        let waking = new_job(owner, None);
        store.insert_agent_job(&waking, Utc::now()).expect("insert");
        assert!(store.session_has_live_owner_wake_job(owner).expect("live"));
        assert!(
            !store
                .session_has_live_owner_wake_job(Uuid::new_v4())
                .expect("other owner")
        );

        let queued = new_job(Uuid::new_v4(), None);
        store
            .insert_agent_job_in_state(&queued, Utc::now(), JobState::Queued)
            .expect("insert queued");
        assert!(
            store
                .session_has_live_owner_wake_job(queued.owner_session_id)
                .expect("queued"),
            "a job held for a deploy still wakes its owner when it settles"
        );

        store
            .settle_agent_job(waking.id, JobState::Succeeded, &done(), true, Utc::now())
            .expect("settle");
        assert!(
            !store
                .session_has_live_owner_wake_job(owner)
                .expect("settled"),
            "once settled the wake row exists and the job no longer counts"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_settle_without_wake_writes_no_scheduled_job() {
        let store = store();
        let owner = Uuid::new_v4();
        let new = new_job(owner, None);
        store.insert_agent_job(&new, Utc::now()).expect("insert");
        let wake = store
            .settle_agent_job(new.id, JobState::Failed, &done(), false, Utc::now())
            .expect("settle");
        assert!(wake.is_none());
        assert!(
            store
                .list_scheduled_jobs()
                .expect("wakes")
                .iter()
                .all(|job| job.wake_session_id != Some(owner))
        );
        assert!(store.list_running_agent_jobs().expect("running").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn terminal_jobs_are_immutable_and_undeletable() {
        let store = store();
        let new = new_job(Uuid::new_v4(), None);
        store.insert_agent_job(&new, Utc::now()).expect("insert");
        store
            .settle_agent_job(new.id, JobState::Lost, &done(), true, Utc::now())
            .expect("settle");
        assert!(
            store
                .conn
                .execute("UPDATE agent_jobs SET state='running'", [])
                .is_err()
        );
        assert!(store.conn.execute("DELETE FROM agent_jobs", []).is_err());
    }

    /// #1333: a lost job is friction; a failed build is ordinary iteration.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn lost_jobs_record_friction_and_failed_builds_do_not() {
        let store = store();
        let owner = Uuid::new_v4();
        let (lost, failed) = (new_job(owner, None), new_job(owner, None));
        store.insert_agent_job(&lost, Utc::now()).expect("insert");
        store.insert_agent_job(&failed, Utc::now()).expect("insert");
        store
            .settle_agent_job(lost.id, JobState::Lost, &done(), true, Utc::now())
            .expect("settle lost");
        store
            .settle_agent_job(failed.id, JobState::Failed, &done(), true, Utc::now())
            .expect("settle failed");
        let rows: Vec<(String, String, String)> = store
            .conn
            .prepare("SELECT signature, session_id, evidence_ref FROM friction_events")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(
                "agent_job:build:lost".to_string(),
                owner.to_string(),
                format!("job:{}", lost.id)
            )]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn listing_is_scoped_to_the_owner_newest_first() {
        let store = store();
        let (mine, other) = (Uuid::new_v4(), Uuid::new_v4());
        let first = new_job(mine, None);
        let second = new_job(mine, None);
        store.insert_agent_job(&first, Utc::now()).expect("insert");
        store.insert_agent_job(&second, Utc::now()).expect("insert");
        store
            .insert_agent_job(&new_job(other, None), Utc::now())
            .expect("insert");
        let listed = store.list_agent_jobs(mine, 10).expect("list");
        let ids: Vec<_> = listed.iter().map(|row| row.job.id).collect();
        assert_eq!(ids, vec![second.id, first.id]);
    }
    /// #1077 review: the rebuild carries the AUTOINCREMENT high-water mark over
    /// (the larger of the old `sqlite_sequence` value and `MAX(sequence)`), so a
    /// sequence number is never reissued: empty table, sequence above the rows,
    /// and the ordinary case.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_cloud_sweep_migration_keeps_the_autoincrement_high_water_mark() {
        use rsi_common::agent_jobs::CloudSweepJobParams;
        for (rows, recorded, expected_next) in [
            (0usize, Some(7i64), 8i64),
            (2, Some(10), 11),
            (3, None, 4),
            (0, None, 1),
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let path = dir.path().join("agent-jobs-sequence.db");
            let store = Store::open(&path).expect("open");
            let owner = Uuid::new_v4();
            for _ in 0..rows {
                store
                    .insert_agent_job(&new_job(owner, None), Utc::now())
                    .expect("insert");
            }
            crate::store::tests::rewind_post_v121_tail_to(
                &store.conn,
                crate::store::AGENT_JOBS_CLOUD_SWEEP_SCHEMA_VERSION - 1,
            );
            if let Some(recorded) = recorded {
                let updated = store
                    .conn
                    .execute(
                        "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'agent_jobs'",
                        [recorded],
                    )
                    .expect("set sequence");
                if updated == 0 {
                    store
                        .conn
                        .execute(
                            "INSERT INTO sqlite_sequence (name, seq) VALUES ('agent_jobs', ?1)",
                            [recorded],
                        )
                        .expect("insert sequence");
                }
            }
            drop(store);
            let upgraded = Store::open(&path).expect("upgrade");
            let mut sweep = new_job(owner, None);
            sweep.params = JobParams::CloudSweep(CloudSweepJobParams {
                sha: "0123456789abcdef0123456789abcdef01234567".into(),
            });
            upgraded
                .insert_agent_job(&sweep, Utc::now())
                .expect("insert after migration");
            let next: i64 = upgraded
                .conn
                .query_row(
                    "SELECT sequence FROM agent_jobs WHERE id = ?1",
                    [sweep.id.to_string()],
                    |row| row.get(0),
                )
                .expect("sequence");
            assert_eq!(
                next, expected_next,
                "rows={rows} recorded={recorded:?}: the next sequence continues the high-water mark"
            );
        }
    }

    /// #1077: the cloud_sweep rebuild keeps every row, index and trigger of a V148
    /// database, then admits `cloud_sweep`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn the_cloud_sweep_migration_keeps_every_row_and_admits_the_new_kind() {
        use rsi_common::agent_jobs::{CloudSweepJobParams, LandingJobParams, TestJobParams};
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("agent-jobs-cloud-sweep.db");
        let store = Store::open(&path).expect("open");
        let owner = Uuid::new_v4();
        let landing = LandingJobParams {
            accepted: "0123456789abcdef0123456789abcdef01234567".into(),
            test_filters: vec!["rsid=agent_jobs".into()],
        };
        let mut ids = Vec::new();
        for (index, params) in [
            JobParams::Build(BuildJobParams {
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
            JobParams::Test(TestJobParams {
                scoped_test: None,
                recipe: None,
                shard: None,
                filterset: None,
                package: Some("rsid".into()),
                filters: vec!["agent_jobs".into()],
                exact: false,
                lib_only: true,
                test_target: None,
                candidate_receipt: None,
                qa_lane: None,
                timeout_minutes: None,
                cpu_quota_percent: None,
            }),
            JobParams::Landing(landing.clone()),
            JobParams::CloudGate(landing),
        ]
        .into_iter()
        .enumerate()
        {
            let mut new = new_job(owner, Some(&format!("key-{index}")));
            new.params = params;
            store.insert_agent_job(&new, Utc::now()).expect("insert");
            ids.push(new.id);
        }
        // One terminal job (with its wake) and three still running.
        store
            .settle_agent_job(ids[1], JobState::Failed, &done(), true, Utc::now())
            .expect("settle");
        let snapshot = |store: &Store| -> Vec<String> {
            let mut statement = store
                .conn
                .prepare(
                    "SELECT sequence, id, owner_session_id, project_id, kind, name, params_json, cwd, \
                     unit_name, log_path, status_path, state, exit_code, result_json, idempotency_key, \
                     wake_job_id, created_at, finished_at, row_version FROM agent_jobs ORDER BY sequence",
                )
                .expect("prepare");
            let count = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..count)
                        .map(|column| {
                            row.get::<_, rusqlite::types::Value>(column)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|values| values.join("|"))
                })
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("rows")
        };
        let before = snapshot(&store);
        assert_eq!(before.len(), 4);

        // Rewind to a genuine V148 database: the old kind CHECK, no cloud_sweep.
        crate::store::tests::rewind_post_v121_tail_to(
            &store.conn,
            crate::store::AGENT_JOBS_CLOUD_SWEEP_SCHEMA_VERSION - 1,
        );
        let table_sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='agent_jobs'",
                [],
                |row| row.get(0),
            )
            .expect("table sql");
        assert!(!table_sql.contains("cloud_sweep"));
        assert_eq!(snapshot(&store), before, "the rewind keeps the rows too");
        let mut sweep = new_job(owner, None);
        sweep.params = JobParams::CloudSweep(CloudSweepJobParams {
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
        });
        assert!(
            store.insert_agent_job(&sweep, Utc::now()).is_err(),
            "a V148 database refuses cloud_sweep"
        );
        drop(store);

        let upgraded = Store::open(&path).expect("upgrade");
        assert_eq!(
            upgraded
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("version"),
            crate::store::LATEST_SCHEMA_VERSION
        );
        assert_eq!(
            snapshot(&upgraded),
            before,
            "every row survives byte for byte"
        );
        for (kind, name) in CATALOG_OBJECTS {
            let present: bool = upgraded
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
                    [kind, name],
                    |row| row.get(0),
                )
                .expect("probe");
            assert!(present, "{kind} {name} is recreated");
        }
        let (row, replayed) = upgraded
            .insert_agent_job(&sweep, Utc::now())
            .expect("cloud_sweep accepted after the migration");
        assert!(!replayed);
        assert_eq!(row.job.kind, JobKind::CloudSweep);
        let sequences: Vec<i64> = upgraded
            .conn
            .prepare("SELECT sequence FROM agent_jobs ORDER BY sequence")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert_eq!(sequences.len(), 5);
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        // Triggers and the owner-key index still hold.
        assert!(upgraded.conn.execute("DELETE FROM agent_jobs", []).is_err());
        assert!(
            upgraded
                .conn
                .execute(
                    "UPDATE agent_jobs SET state='running' WHERE state='failed'",
                    []
                )
                .is_err()
        );
        let mut duplicate = new_job(owner, Some("key-0"));
        duplicate.name = Some("different".into());
        assert!(upgraded.insert_agent_job(&duplicate, Utc::now()).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn v159_keeps_legacy_jobs_results_wakes_and_sequence() {
        let store = store();
        let owner = Uuid::new_v4();
        let mut expected = Vec::new();
        for (index, state) in [
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Lost,
        ]
        .into_iter()
        .enumerate()
        {
            let new = new_job(owner, Some(&format!("legacy-{index}")));
            store.insert_agent_job(&new, Utc::now()).unwrap();
            if state.is_terminal() {
                store
                    .settle_agent_job(new.id, state, &done(), true, Utc::now())
                    .unwrap();
            }
            let mut row = store.get_agent_job(new.id).unwrap().unwrap().job;
            row.started_at = None;
            expected.push(row);
        }
        let wakes = store.list_scheduled_jobs().unwrap();
        // A genuine V158 job table, without the V159 column or state CHECK.
        crate::store::rebuild_agent_jobs(&store.conn, crate::store::AGENT_JOBS_CLOUD_SWEEP_TABLE)
            .unwrap();
        store.conn.pragma_update(None, "user_version", 158).unwrap();
        store
            .conn
            .execute(
                "UPDATE sqlite_sequence SET seq=100 WHERE name='agent_jobs'",
                [],
            )
            .unwrap();

        store.migrate_v159(158).unwrap();
        for old in expected {
            let restored = store.get_agent_job(old.id).unwrap().unwrap().job;
            assert_eq!(
                serde_json::to_value(restored).unwrap(),
                serde_json::to_value(old).unwrap(),
                "legacy timing falls back to created_at; all job data survives"
            );
        }
        assert_eq!(store.list_scheduled_jobs().unwrap().len(), wakes.len());
        let next = new_job(owner, Some("after-upgrade"));
        store
            .insert_agent_job_in_state(&next, Utc::now(), JobState::Queued)
            .unwrap();
        let sequence: i64 = store
            .conn
            .query_row(
                "SELECT sequence FROM agent_jobs WHERE id=?1",
                [next.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sequence, 101);
        assert!(
            store
                .conn
                .execute("DELETE FROM agent_jobs WHERE id=?1", [next.id.to_string()])
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_queued_job_starts_once_settles_from_queued_and_never_counts_as_running() {
        let store = store();
        let owner = Uuid::new_v4();
        let (row, _) = store
            .insert_agent_job_in_state(&new_job(owner, None), Utc::now(), JobState::Queued)
            .unwrap();
        assert_eq!(row.job.state, JobState::Queued);
        assert_eq!(row.job.held.as_deref(), Some("held_for_deploy"));
        assert_eq!(row.job.started_at, None);
        assert!(store.list_running_agent_jobs().unwrap().is_empty());
        assert_eq!(store.list_queued_agent_jobs().unwrap().len(), 1);
        assert!(!JobState::Queued.is_terminal());

        assert!(
            store
                .start_queued_agent_job(row.job.id, Utc::now())
                .unwrap()
        );
        assert!(
            !store
                .start_queued_agent_job(row.job.id, Utc::now())
                .unwrap(),
            "the start is a CAS"
        );
        let started = store.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(started.state, JobState::Running);
        assert_eq!(started.held, None);
        assert!(started.started_at.is_some());
        assert_eq!(store.list_running_agent_jobs().unwrap().len(), 1);

        let (queued, _) = store
            .insert_agent_job_in_state(&new_job(owner, None), Utc::now(), JobState::Queued)
            .unwrap();
        let settled = store
            .settle_agent_job_outcome(
                queued.job.id,
                JobState::Failed,
                &AgentJobResultV1::default(),
                false,
                Utc::now(),
            )
            .unwrap();
        assert!(settled.is_some(), "a queued job can be cancelled");
        assert!(
            !store
                .start_queued_agent_job(queued.job.id, Utc::now())
                .unwrap(),
            "a settled job never starts"
        );
    }
}
