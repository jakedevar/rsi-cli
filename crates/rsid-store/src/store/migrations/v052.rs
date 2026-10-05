impl Store {
    fn migrate_v052(&self, version: i32) -> Result<()> {
        // V52: Recursive DAG cancellation requests.
        //
        // This is Phase 5A.2's durable cancellation control plane for the
        // fake scheduler. It models graph/run/task scopes, records request
        // observation/application, and links cancelled scheduler runs back to
        // the request. It does not add leases, RPCs, TUI surfaces, live
        // interrupt handles, or a background executor loop.
        if version < 52 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_cancellation_requests (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scope TEXT NOT NULL CHECK (scope IN ('graph', 'run', 'task')),
                    status TEXT NOT NULL CHECK (status IN (
                        'requested', 'observed', 'applied', 'rejected'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'manual_rpc', 'test_harness', 'startup_recovery'
                    )),
                    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
                    requested_by TEXT,
                    requested_at TEXT NOT NULL,
                    observed_at TEXT,
                    applied_at TEXT,
                    rejection_reason TEXT,
                    CHECK (
                        (scope = 'graph' AND run_id IS NULL AND task_id IS NULL)
                        OR (scope = 'run' AND run_id IS NOT NULL AND task_id IS NULL)
                        OR (scope = 'task' AND run_id IS NULL AND task_id IS NOT NULL)
                    ),
                    CHECK (status != 'observed' OR observed_at IS NOT NULL),
                    CHECK (status != 'applied' OR applied_at IS NOT NULL),
                    CHECK (status != 'rejected' OR rejection_reason IS NOT NULL)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_graph_open
                    ON recursive_cancellation_requests(graph_id, requested_at, id)
                    WHERE status IN ('requested', 'observed');
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_run_open
                    ON recursive_cancellation_requests(run_id, requested_at, id)
                    WHERE status IN ('requested', 'observed');
                CREATE INDEX IF NOT EXISTS idx_recursive_cancel_task_open
                    ON recursive_cancellation_requests(graph_id, task_id, requested_at, id)
                    WHERE status IN ('requested', 'observed');",
            )?;
            self.add_column_if_not_exists(
                "recursive_scheduler_runs",
                "cancellation_request_id",
                "TEXT REFERENCES recursive_cancellation_requests(id)",
            )?;
            self.add_column_if_not_exists(
                "recursive_scheduler_runs",
                "cancellation_reason",
                "TEXT",
            )?;
            tracing::info!("V52 migration complete: recursive DAG cancellation requests");
            self.conn.pragma_update(None, "user_version", 52)?;
        }

        Ok(())
    }
}
