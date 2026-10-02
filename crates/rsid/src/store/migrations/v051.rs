impl Store {
    fn migrate_v051(&self, version: i32) -> Result<()> {
        // V51: Recursive DAG scheduler run records.
        //
        // This is Phase 5A.1's durable invocation audit. It records explicit
        // fake scheduler runs and links completed runs to their
        // `scheduler-report` artifact without adding cancellation,
        // leases/concurrency enforcement, recovery passes, RPC controls, or
        // background scheduling.
        if version < 51 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_scheduler_runs (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'cancelling', 'completed', 'cancelled',
                        'failed', 'rejected', 'lease_expired'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'test_harness', 'manual_rpc', 'startup_recovery', 'future_daemon_loop'
                    )),
                    operator TEXT,
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'graph_terminal', 'idle_no_runnable', 'step_limit_exceeded',
                            'partial_failure', 'quarantined', 'cancellation_requested',
                            'lease_expired', 'executor_error', 'recovery_deferred'
                        )
                    ),
                    step_count INTEGER NOT NULL DEFAULT 0 CHECK (step_count >= 0),
                    max_steps INTEGER NOT NULL CHECK (max_steps > 0),
                    executor_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_mode IN ('fake', 'live_session')
                    ),
                    failure_reason TEXT,
                    report_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    CHECK (
                        (status IN ('running', 'cancelling') AND completed_at IS NULL)
                        OR (status NOT IN ('running', 'cancelling') AND completed_at IS NOT NULL)
                    )
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_graph_started
                    ON recursive_scheduler_runs(graph_id, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_status
                    ON recursive_scheduler_runs(status, started_at DESC);

                CREATE TABLE IF NOT EXISTS recursive_scheduler_run_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    event_type TEXT NOT NULL,
                    message TEXT,
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_run_events_run
                    ON recursive_scheduler_run_events(run_id, id);",
            )?;
            tracing::info!("V51 migration complete: recursive DAG scheduler run records");
            self.conn.pragma_update(None, "user_version", 51)?;
        }

        Ok(())
    }
}
