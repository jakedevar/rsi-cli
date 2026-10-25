// rsi-migration: additive
/// #3178: the bench run store. `bench_runs` is one admitted benchmark run: the
/// pinned config, dataset, task set (with each task's pinned image digests),
/// model, effort, budget, per-task USD cap and concurrency. `bench_tasks` is
/// one trial of it: the container binding, the session, the state machine, the
/// final usage (from the increment SQL, never the session-cumulative rows), the
/// build digest, the live trace and its sha256, and the route verdict.
///
/// Neither table is ever deleted from (the triggers refuse it). A task row only
/// moves forward: a settled state, the session, the trace hash and the identity
/// columns never change once written.
impl Store {
    fn migrate_v186(&self, version: i32) -> Result<()> {
        if version < 186 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE bench_runs (
                    id TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(id)),
                    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
                    job_id TEXT,
                    config_path TEXT NOT NULL,
                    config_digest TEXT NOT NULL,
                    dataset_commit TEXT NOT NULL,
                    docker_host TEXT NOT NULL,
                    model TEXT NOT NULL,
                    effort TEXT NOT NULL,
                    budget_usd REAL NOT NULL CHECK(budget_usd > 0),
                    per_task_cap_usd REAL NOT NULL CHECK(per_task_cap_usd > 0),
                    concurrency INTEGER NOT NULL CHECK(concurrency BETWEEN 1 AND 64),
                    tasks_json TEXT NOT NULL CHECK(json_valid(tasks_json)),
                    loop_knobs_json TEXT NOT NULL CHECK(json_valid(loop_knobs_json)),
                    state TEXT NOT NULL CHECK(state IN ('active','finished','cancelled')),
                    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
                    finished_at TEXT CHECK(finished_at IS NULL OR rsi_rfc3339_nanos_is_canonical(finished_at))
                );
                CREATE INDEX bench_runs_active ON bench_runs(state) WHERE state = 'active';
                CREATE TRIGGER bench_runs_no_delete
                    BEFORE DELETE ON bench_runs
                    BEGIN SELECT RAISE(ABORT,'bench runs are retained'); END;
                CREATE TRIGGER bench_runs_forward
                    BEFORE UPDATE ON bench_runs
                    WHEN NEW.id IS NOT OLD.id
                        OR NEW.project_id IS NOT OLD.project_id
                        OR NEW.config_digest IS NOT OLD.config_digest
                        OR NEW.dataset_commit IS NOT OLD.dataset_commit
                        OR NEW.docker_host IS NOT OLD.docker_host
                        OR NEW.model IS NOT OLD.model
                        OR NEW.effort IS NOT OLD.effort
                        OR NEW.budget_usd IS NOT OLD.budget_usd
                        OR NEW.per_task_cap_usd IS NOT OLD.per_task_cap_usd
                        OR NEW.concurrency IS NOT OLD.concurrency
                        OR NEW.tasks_json IS NOT OLD.tasks_json
                        OR NEW.created_at IS NOT OLD.created_at
                        OR (OLD.state <> 'active' AND NEW.state IS NOT OLD.state)
                    BEGIN SELECT RAISE(ABORT,'bench run pins are immutable'); END;
                CREATE TABLE bench_tasks (
                    run_id TEXT NOT NULL REFERENCES bench_runs(id) ON DELETE RESTRICT,
                    trial TEXT NOT NULL,
                    task TEXT NOT NULL,
                    attempt INTEGER NOT NULL CHECK(attempt >= 1),
                    session_id TEXT UNIQUE REFERENCES sessions(id) ON DELETE RESTRICT
                        CHECK(session_id IS NULL OR rsi_uuid_is_canonical(session_id)),
                    compose_project TEXT NOT NULL,
                    container TEXT NOT NULL,
                    workdir TEXT NOT NULL,
                    user TEXT,
                    agent_timeout_sec INTEGER NOT NULL CHECK(agent_timeout_sec > 0),
                    request_digest TEXT NOT NULL,
                    state TEXT NOT NULL
                        CHECK(state IN ('starting','running','finished','cancelled','infra_interrupted')),
                    stop_reason TEXT,
                    cost_usd REAL,
                    prompt_tokens INTEGER,
                    cached_tokens INTEGER,
                    output_tokens INTEGER,
                    reasoning_tokens INTEGER,
                    wall_ms INTEGER,
                    harness_build_digest TEXT,
                    trace_path TEXT,
                    trace_sha256 TEXT,
                    route_verdict TEXT CHECK(route_verdict IS NULL OR route_verdict IN ('valid','invalid_route')),
                    route_violations_json TEXT CHECK(route_violations_json IS NULL OR json_valid(route_violations_json)),
                    started_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(started_at)),
                    finished_at TEXT CHECK(finished_at IS NULL OR rsi_rfc3339_nanos_is_canonical(finished_at)),
                    PRIMARY KEY(run_id, trial)
                ) WITHOUT ROWID;
                CREATE INDEX bench_tasks_live ON bench_tasks(run_id) WHERE state IN ('starting','running');
                CREATE TRIGGER bench_tasks_no_delete
                    BEFORE DELETE ON bench_tasks
                    BEGIN SELECT RAISE(ABORT,'bench tasks are retained'); END;
                CREATE TRIGGER bench_tasks_forward
                    BEFORE UPDATE ON bench_tasks
                    WHEN NEW.run_id IS NOT OLD.run_id
                        OR NEW.trial IS NOT OLD.trial
                        OR NEW.task IS NOT OLD.task
                        OR NEW.attempt IS NOT OLD.attempt
                        OR NEW.container IS NOT OLD.container
                        OR NEW.request_digest IS NOT OLD.request_digest
                        OR (OLD.session_id IS NOT NULL AND NEW.session_id IS NOT OLD.session_id)
                        OR (OLD.trace_sha256 IS NOT NULL AND NEW.trace_sha256 IS NOT OLD.trace_sha256)
                        OR (OLD.state NOT IN ('starting','running') AND NEW.state IS NOT OLD.state)
                        OR (OLD.state = 'running' AND NEW.state = 'starting')
                    BEGIN SELECT RAISE(ABORT,'bench task rows only move forward'); END;",
            )?;
            tx.pragma_update(None, "user_version", 186)?;
            tx.commit()?;
        }
        Ok(())
    }
}
