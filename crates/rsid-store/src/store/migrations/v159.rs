/// Issue #1566: `AgentSubmitJob` during a deploy drain is held, not refused.
/// `agent_jobs.state` admits `queued` (accepted, no unit launched yet) and the
/// table gains `started_at` (when the unit was launched; NULL while queued and
/// for rows that ran before this step, which read their `created_at`). Every
/// other column, constraint, index and trigger is the V149 catalog's.
// RSI-RELEASED-MIGRATION-BEGIN: v159-agent-jobs-queued
const V159_AGENT_JOBS_TABLE: &str = "CREATE TABLE agent_jobs (
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
// RSI-RELEASED-MIGRATION-END: v159-agent-jobs-queued

impl Store {
    fn migrate_v159(&self, version: i32) -> Result<()> {
        if version < 159 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if prior != 158 {
                return Err(DaemonError::Store(format!(
                    "the queued agent job state requires V158, found V{prior}"
                )));
            }
            rebuild_agent_jobs(&tx, V159_AGENT_JOBS_TABLE)?;
            tx.pragma_update(None, "user_version", 159)?;
            tx.commit()?;
        }
        Ok(())
    }
}
