impl Store {
    fn migrate_v054(&self, version: i32) -> Result<()> {
        // V54: Recursive DAG budgeted recovery passes.
        //
        // Phase 5A.4 makes daemon-startup recovery bounded and inspectable.
        // Recovery remains explicit and synchronous; this schema records pass
        // budgets, counters, stop reasons, and the graph-level deferred state
        // needed for later operator readback without adding a background loop.
        if version < 54 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_recovery_passes (
                    id TEXT PRIMARY KEY,
                    source TEXT NOT NULL CHECK (source IN (
                        'startup', 'manual_rpc', 'test_harness'
                    )),
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'completed', 'deferred', 'failed'
                    )),
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    max_graphs INTEGER NOT NULL CHECK (max_graphs > 0),
                    time_budget_ms INTEGER CHECK (
                        time_budget_ms IS NULL OR time_budget_ms >= 0
                    ),
                    checked INTEGER NOT NULL DEFAULT 0 CHECK (checked >= 0),
                    recovered INTEGER NOT NULL DEFAULT 0 CHECK (recovered >= 0),
                    quarantined INTEGER NOT NULL DEFAULT 0 CHECK (quarantined >= 0),
                    deferred INTEGER NOT NULL DEFAULT 0 CHECK (deferred >= 0),
                    skipped INTEGER NOT NULL DEFAULT 0 CHECK (skipped >= 0),
                    errors INTEGER NOT NULL DEFAULT 0 CHECK (errors >= 0),
                    last_graph_id TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'completed', 'max_graphs', 'time_budget', 'store_error'
                        )
                    ),
                    error TEXT,
                    CHECK (
                        (status = 'running' AND completed_at IS NULL)
                        OR (status <> 'running' AND completed_at IS NOT NULL)
                    )
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_started
                    ON recursive_recovery_passes(started_at DESC, id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_passes_status
                    ON recursive_recovery_passes(status, started_at DESC);

                CREATE TABLE IF NOT EXISTS recursive_recovery_deferred_graphs (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    pass_id TEXT NOT NULL REFERENCES recursive_recovery_passes(id) ON DELETE CASCADE,
                    deferred_at TEXT NOT NULL,
                    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
                    next_after TEXT,
                    last_attempted_at TEXT,
                    last_error TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_deferred_due
                    ON recursive_recovery_deferred_graphs(next_after, deferred_at, graph_id);

                CREATE TABLE IF NOT EXISTS recursive_recovery_graph_states (
                    graph_id TEXT PRIMARY KEY REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    state TEXT NOT NULL CHECK (state IN (
                        'pending_recovery', 'recovered', 'quarantined', 'deferred'
                    )),
                    pass_id TEXT REFERENCES recursive_recovery_passes(id) ON DELETE SET NULL,
                    last_attempted_at TEXT,
                    completed_at TEXT,
                    deferred_at TEXT,
                    reason TEXT,
                    last_error TEXT,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_recovery_graph_states_state
                    ON recursive_recovery_graph_states(state, updated_at DESC);",
            )?;
            tracing::info!("V54 migration complete: recursive DAG budgeted recovery passes");
            self.conn.pragma_update(None, "user_version", 54)?;
        }

        Ok(())
    }
}
