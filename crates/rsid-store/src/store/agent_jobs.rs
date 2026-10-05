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
    AgentJobResultV1, AgentJobV1, JOB_KEY_CONFLICT, JobKind, JobParams, JobState, JobWake,
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

const COLUMNS: &str = "id, owner_session_id, kind, name, params_json, cwd, unit_name, log_path, \
    status_path, state, exit_code, result_json, created_at, finished_at";

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
/// before the typed params are decoded.
fn decode_params(text: &str) -> serde_json::Result<(JobParams, JobWake)> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    let wake = match value
        .as_object_mut()
        .and_then(|object| object.remove("wake"))
    {
        Some(wake) => serde_json::from_value(wake)?,
        None => JobWake::Owner,
    };
    Ok((serde_json::from_value(value)?, wake))
}

fn encode_params(params: &JobParams, wake: JobWake) -> serde_json::Result<String> {
    let mut value = serde_json::to_value(params)?;
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
    let (params, wake) =
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
            finished_at: row.get(13)?,
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
    /// Record a new `running` job. A replay of the same `(owner, key)` with the
    /// same kind, params and name returns the original job and `replayed=true`;
    /// a different request under the same key is refused.
    pub fn insert_agent_job(
        &self,
        new: &NewAgentJob,
        now: DateTime<Utc>,
    ) -> Result<(AgentJobRow, bool)> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let params_json =
            encode_params(&new.params, new.wake).map_err(|e| DaemonError::Store(e.to_string()))?;
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
        tx.execute(
            "INSERT INTO agent_jobs(id, owner_session_id, project_id, kind, name, params_json, \
             cwd, unit_name, log_path, status_path, state, idempotency_key, created_at, \
             row_version) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'running',?11,?12,1)",
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
                &format!("SELECT {COLUMNS} FROM agent_jobs WHERE id=?1 AND state='running'"),
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
             finished_at=?6, row_version=row_version+1 WHERE id=?1 AND state='running'",
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
            }),
            JobParams::Test(TestJobParams {
                shard: None,
                filterset: None,
                package: Some("rsid".into()),
                filters: vec!["agent_jobs".into()],
                lib_only: true,
                candidate_receipt: None,
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
}
