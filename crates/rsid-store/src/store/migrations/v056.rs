impl Store {
    fn migrate_v056(&self, version: i32) -> Result<()> {
        // V56: Recursive DAG recovery pass tables for time-budgeted recovery
        if version < 56 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_recovery_passes (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL CHECK (source IN ('startup', 'manual_rpc', 'test_harness')),
                    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'deferred', 'failed')),
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    max_graphs INTEGER NOT NULL CHECK (max_graphs > 0),
                    time_budget_ms INTEGER NOT NULL CHECK (time_budget_ms > 0),
                    checked INTEGER NOT NULL DEFAULT 0,
                    recovered INTEGER NOT NULL DEFAULT 0,
                    quarantined INTEGER NOT NULL DEFAULT 0,
                    deferred INTEGER NOT NULL DEFAULT 0,
                    last_graph_id TEXT,
                    stop_reason TEXT,
                    error TEXT
                );

                CREATE TABLE IF NOT EXISTS recursive_recovery_deferred_graphs (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    pass_id TEXT NOT NULL REFERENCES recursive_recovery_passes(id) ON DELETE CASCADE,
                    deferred_at TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    next_after TEXT,
                    last_error TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_source_started
                    ON recursive_recovery_passes(source, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_status
                    ON recursive_recovery_passes(status, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_deferred_next_after
                    ON recursive_recovery_deferred_graphs(next_after, deferred_at)
                    WHERE next_after IS NOT NULL;",
            )?;
            tracing::info!("V56 migration complete: recursive DAG recovery pass tables");
            self.conn.pragma_update(None, "user_version", 56)?;
        }

        Ok(())
    }
}
