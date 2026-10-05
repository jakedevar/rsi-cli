/// Provisional schema version of the `cloud_sweep` job kind (#1077); the lander
/// renumbers it.
pub(crate) const AGENT_JOBS_CLOUD_SWEEP_SCHEMA_VERSION: i32 = 149;

/// `agent_jobs` with the `cloud_sweep` kind admitted. Columns, order and every
/// other constraint are the V144 catalog's.
pub(crate) const AGENT_JOBS_CLOUD_SWEEP_TABLE: &str = "CREATE TABLE agent_jobs (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(id)),
    owner_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(owner_session_id)),
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    kind TEXT NOT NULL CHECK(kind IN ('test','build','landing','cloud_gate','cloud_sweep')),
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
);";

/// The V144 indexes and triggers, recreated exactly after a rebuild.
pub(crate) const AGENT_JOBS_CLOUD_SWEEP_OBJECTS: &str = "CREATE UNIQUE INDEX agent_jobs_owner_key ON agent_jobs(owner_session_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX agent_jobs_by_state ON agent_jobs(state, sequence);
CREATE INDEX agent_jobs_by_owner ON agent_jobs(owner_session_id, sequence);
CREATE TRIGGER agent_jobs_no_delete BEFORE DELETE ON agent_jobs BEGIN SELECT RAISE(ABORT,'agent jobs are append only'); END;
CREATE TRIGGER agent_jobs_terminal_immutable BEFORE UPDATE ON agent_jobs WHEN OLD.state IN ('succeeded','failed','lost') BEGIN SELECT RAISE(ABORT,'terminal agent jobs are immutable'); END;";

/// Rebuild `agent_jobs` from `table_ddl` (a `CREATE TABLE agent_jobs`), keeping
/// every row (with its `sequence`), the indexes, the triggers and the
/// AUTOINCREMENT high-water mark. The rows are copied while the append-only
/// triggers are gone, then the triggers come back. Dropping the old table
/// deletes its `sqlite_sequence` row, so the larger of that value and
/// `MAX(sequence)` is written back for the new table (also when it is empty).
pub(crate) fn rebuild_agent_jobs(conn: &Connection, table_ddl: &str) -> Result<()> {
    let old_sequence: i64 = conn.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'agent_jobs'), 0)",
        [],
        |row| row.get(0),
    )?;
    conn.execute_batch(&format!(
        "DROP TRIGGER agent_jobs_no_delete;
         DROP TRIGGER agent_jobs_terminal_immutable;
         DROP INDEX agent_jobs_owner_key;
         DROP INDEX agent_jobs_by_state;
         DROP INDEX agent_jobs_by_owner;
         ALTER TABLE agent_jobs RENAME TO agent_jobs_rebuild_old;
         {table_ddl}
         INSERT INTO agent_jobs (sequence, id, owner_session_id, project_id, kind, name, params_json, cwd, unit_name, log_path, status_path, state, exit_code, result_json, idempotency_key, wake_job_id, created_at, finished_at, row_version)
           SELECT sequence, id, owner_session_id, project_id, kind, name, params_json, cwd, unit_name, log_path, status_path, state, exit_code, result_json, idempotency_key, wake_job_id, created_at, finished_at, row_version
           FROM agent_jobs_rebuild_old ORDER BY sequence;
         DROP TABLE agent_jobs_rebuild_old;
         {AGENT_JOBS_CLOUD_SWEEP_OBJECTS}"
    ))?;
    let high_water: i64 = conn.query_row(
        "SELECT MAX(?1, COALESCE((SELECT MAX(sequence) FROM agent_jobs), 0))",
        [old_sequence],
        |row| row.get(0),
    )?;
    if high_water > 0 {
        let updated = conn.execute(
            "UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'agent_jobs'",
            [high_water],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES ('agent_jobs', ?1)",
                [high_water],
            )?;
        }
    }
    Ok(())
}

impl Store {
    fn migrate_v149(&self, version: i32) -> Result<()> {
        // Issue #1077: the `cloud_sweep` job kind. The version is provisional;
        // the lander assigns the final number.
        if version < 149 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if prior != AGENT_JOBS_CLOUD_SWEEP_SCHEMA_VERSION - 1 {
                return Err(DaemonError::Store(format!(
                    "the cloud_sweep job kind requires V{}, found V{prior}",
                    AGENT_JOBS_CLOUD_SWEEP_SCHEMA_VERSION - 1
                )));
            }
            rebuild_agent_jobs(&tx, AGENT_JOBS_CLOUD_SWEEP_TABLE)?;
            tx.pragma_update(None, "user_version", AGENT_JOBS_CLOUD_SWEEP_SCHEMA_VERSION)?;
            tx.commit()?;
        }

        Ok(())
    }
}
