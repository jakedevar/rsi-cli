impl Store {
    fn migrate_v055(&self, version: i32) -> Result<()> {
        // V55: Recursive DAG live attempt/session correlation.
        //
        // Phase 6.1 is schema/read-model only. This records a durable
        // correlation between a future live recursive attempt and the
        // scheduler run, recursive attempt, optional RSI session, provider,
        // model, and sandbox snapshots. Graphs and scheduler runs remain
        // fake-only; this table does not launch sessions or enable live mode.
        if version < 55 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_live_attempts (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    attempt_id TEXT NOT NULL UNIQUE REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                    session_id TEXT REFERENCES sessions(id),
                    provider TEXT CHECK (
                        provider IS NULL OR provider IN (
                            'Claude', 'Codex', 'Local', 'Antigravity', 'Gemini', 'CodexAppServer', 'Harness'
                        )
                    ),
                    model TEXT CHECK (model IS NULL OR length(trim(model)) > 0),
                    sandbox_kind TEXT CHECK (
                        sandbox_kind IS NULL OR sandbox_kind IN ('None', 'GitWorktree')
                    ),
                    sandbox_root TEXT,
                    sandbox_branch TEXT,
                    sandbox_worktree_id TEXT,
                    workflow_id TEXT REFERENCES workflows(id),
                    topology_id TEXT REFERENCES topologies(id),
                    workflow_execution_id TEXT,
                    topology_workflow_id TEXT REFERENCES workflows(id),
                    execution_mode TEXT NOT NULL CHECK (execution_mode IN ('live_session')),
                    status TEXT NOT NULL CHECK (status IN (
                        'created', 'launching', 'running', 'waiting_approval',
                        'succeeded', 'decomposed', 'failed', 'blocked',
                        'cancelled', 'interrupted', 'lost', 'recovery_pending'
                    )),
                    recovery_status TEXT NOT NULL DEFAULT 'none' CHECK (
                        recovery_status IN ('none', 'pending', 'recovered', 'lost', 'quarantined')
                    ),
                    prompt_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    diff_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    test_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                    lease_owner TEXT,
                    lease_token TEXT,
                    heartbeat_at TEXT,
                    lease_expires_at TEXT,
                    max_wall_time_ms INTEGER CHECK (
                        max_wall_time_ms IS NULL OR max_wall_time_ms > 0
                    ),
                    created_at TEXT NOT NULL,
                    started_at TEXT,
                    launched_at TEXT,
                    completed_at TEXT,
                    recovery_checked_at TEXT,
                    recovered_at TEXT,
                    failure_reason TEXT,
                    interruption_reason TEXT,
                    cancellation_reason TEXT,
                    recovery_reason TEXT,
                    error TEXT,
                    updated_at TEXT NOT NULL,
                    CHECK (
                        (
                            status IN (
                                'succeeded', 'decomposed', 'failed', 'blocked',
                                'cancelled', 'interrupted', 'lost'
                            )
                            AND completed_at IS NOT NULL
                        )
                        OR (
                            status NOT IN (
                                'succeeded', 'decomposed', 'failed', 'blocked',
                                'cancelled', 'interrupted', 'lost'
                            )
                            AND completed_at IS NULL
                        )
                    ),
                    UNIQUE (graph_id, task_id, phase, attempt_no)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_graph
                    ON recursive_live_attempts(graph_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_task
                    ON recursive_live_attempts(task_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_run
                    ON recursive_live_attempts(scheduler_run_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_attempts_status
                    ON recursive_live_attempts(status, updated_at DESC, id);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_live_attempts_session
                    ON recursive_live_attempts(session_id)
                    WHERE session_id IS NOT NULL;",
            )?;
            tracing::info!("V55 migration complete: recursive DAG live correlation schema");
            self.conn.pragma_update(None, "user_version", 55)?;
        }

        Ok(())
    }
}
