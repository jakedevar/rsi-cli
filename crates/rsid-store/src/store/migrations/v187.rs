// rsi-migration: contract
/// #3181: `agent_jobs.kind` admits `bench` (one pinned benchmark run). The table
/// is rebuilt only to widen the kind CHECK: every row keeps its `sequence`,
/// every column (including `started_at`), the indexes, the append-only and
/// terminal-immutable triggers and the AUTOINCREMENT high-water mark. No row is
/// dropped or rewritten.
const V187_AGENT_JOBS_TABLE: &str = "CREATE TABLE agent_jobs (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE CHECK(rsi_uuid_is_canonical(id)),
    owner_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(owner_session_id)),
    project_id TEXT CHECK(project_id IS NULL OR rsi_uuid_is_canonical(project_id)),
    kind TEXT NOT NULL CHECK(kind IN ('test','build','landing','cloud_gate','cloud_sweep','bench')),
    name TEXT CHECK(name IS NULL OR length(name)>0),
    params_json TEXT NOT NULL CHECK(json_valid(params_json)),
    cwd TEXT NOT NULL CHECK(length(cwd)>0),
    unit_name TEXT NOT NULL UNIQUE CHECK(length(unit_name)>0),
    log_path TEXT NOT NULL CHECK(length(log_path)>0),
    status_path TEXT NOT NULL CHECK(length(status_path)>0),
    state TEXT NOT NULL CHECK(state IN ('queued','running','succeeded','failed','lost')),
    exit_code INTEGER,
    result_json TEXT CHECK(result_json IS NULL OR json_valid(result_json)),
    idempotency_key TEXT CHECK(idempotency_key IS NULL OR length(idempotency_key)>0),
    wake_job_id TEXT UNIQUE CHECK(wake_job_id IS NULL OR rsi_uuid_is_canonical(wake_job_id)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    finished_at TEXT CHECK(finished_at IS NULL OR rsi_rfc3339_nanos_is_canonical(finished_at)),
    row_version INTEGER NOT NULL CHECK(row_version>0),
    started_at TEXT CHECK(started_at IS NULL OR rsi_rfc3339_nanos_is_canonical(started_at))
);";

impl Store {
    fn migrate_v187(&self, version: i32) -> Result<()> {
        if version < 187 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let old_sequence: i64 = tx.query_row(
                "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'agent_jobs'), 0)",
                [],
                |row| row.get(0),
            )?;
            tx.execute_batch(&format!(
                "DROP TRIGGER agent_jobs_no_delete;
                 DROP TRIGGER agent_jobs_terminal_immutable;
                 DROP INDEX agent_jobs_owner_key;
                 DROP INDEX agent_jobs_by_state;
                 DROP INDEX agent_jobs_by_owner;
                 ALTER TABLE agent_jobs RENAME TO agent_jobs_rebuild_old;
                 {V187_AGENT_JOBS_TABLE}
                 INSERT INTO agent_jobs (sequence, id, owner_session_id, project_id, kind, name, params_json, cwd, unit_name, log_path, status_path, state, exit_code, result_json, idempotency_key, wake_job_id, created_at, finished_at, row_version, started_at)
                   SELECT sequence, id, owner_session_id, project_id, kind, name, params_json, cwd, unit_name, log_path, status_path, state, exit_code, result_json, idempotency_key, wake_job_id, created_at, finished_at, row_version, started_at
                   FROM agent_jobs_rebuild_old ORDER BY sequence;
                 DROP TABLE agent_jobs_rebuild_old;
                 {AGENT_JOBS_CLOUD_SWEEP_OBJECTS}"
            ))?;
            // Dropping the old table deleted its sqlite_sequence row: write the
            // larger of it and MAX(sequence) back so ids never repeat.
            let high_water: i64 = tx.query_row(
                "SELECT MAX(?1, COALESCE((SELECT MAX(sequence) FROM agent_jobs), 0))",
                [old_sequence],
                |row| row.get(0),
            )?;
            if high_water > 0 {
                let updated = tx.execute(
                    "UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'agent_jobs'",
                    [high_water],
                )?;
                if updated == 0 {
                    tx.execute(
                        "INSERT INTO sqlite_sequence (name, seq) VALUES ('agent_jobs', ?1)",
                        [high_water],
                    )?;
                }
            }
            tx.pragma_update(None, "user_version", 187)?;
            tx.commit()?;
        }
        Ok(())
    }
}
