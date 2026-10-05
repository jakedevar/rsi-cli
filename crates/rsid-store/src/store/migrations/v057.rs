impl Store {
    fn migrate_v057(&self, version: i32) -> Result<()> {
        // V57: Recursive DAG live interrupt ownership.
        //
        // Phase 6.3 records one durable interrupt handle per live attempt. It
        // links graph/run cancellation requests to the live attempt/session
        // interrupt path, but does not expose RPC/TUI controls, add force-kill
        // escalation, heartbeat, crash recovery, or live scheduler reachability.
        if version < 57 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_live_interrupts (
                    id TEXT PRIMARY KEY,
                    live_attempt_id TEXT NOT NULL UNIQUE REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    attempt_id TEXT NOT NULL REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    session_id TEXT REFERENCES sessions(id),
                    cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                    status TEXT NOT NULL CHECK (status IN (
                        'requested', 'sent', 'interrupted', 'failed',
                        'rejected', 'ignored'
                    )),
                    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
                    failure_reason TEXT,
                    requested_at TEXT NOT NULL,
                    sent_at TEXT,
                    completed_at TEXT,
                    CHECK (status != 'sent' OR sent_at IS NOT NULL),
                    CHECK (
                        (status IN ('interrupted', 'failed', 'rejected', 'ignored')
                            AND completed_at IS NOT NULL)
                        OR (status IN ('requested', 'sent') AND completed_at IS NULL)
                    ),
                    CHECK (
                        status NOT IN ('failed', 'rejected')
                        OR failure_reason IS NOT NULL
                    )
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_live_attempt
                    ON recursive_live_interrupts(live_attempt_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_status
                    ON recursive_live_interrupts(status, requested_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_cancellation
                    ON recursive_live_interrupts(cancellation_request_id)
                    WHERE cancellation_request_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_live_interrupts_session
                    ON recursive_live_interrupts(session_id)
                    WHERE session_id IS NOT NULL;",
            )?;
            tracing::info!("V57 migration complete: recursive DAG live interrupt handles");
            self.conn.pragma_update(None, "user_version", 57)?;
        }

        Ok(())
    }
}
