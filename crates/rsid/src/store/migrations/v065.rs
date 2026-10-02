impl Store {
    fn migrate_v065(&self, version: i32) -> Result<()> {
        // V65: Recursive DAG live scheduler store primitives.
        //
        // This migration admits the persisted `live_session` mode for graphs,
        // scheduler runs, and task attempts, and adds live-run request/policy
        // metadata columns. No RPC route is registered here and no session
        // launch path is made reachable; store APIs still decide which callers
        // may write live rows.
        if version < 65 {
            self.conn.execute_batch(
                "PRAGMA foreign_keys=OFF;
                BEGIN;

                CREATE TABLE recursive_task_graphs_v65_new (
                    id TEXT PRIMARY KEY,
                    root_task_id TEXT NOT NULL,
                    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
                    objective TEXT NOT NULL CHECK (length(trim(objective)) > 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'active', 'terminal', 'blocked', 'failed', 'cancelled', 'malformed'
                    )),
                    project_id TEXT REFERENCES projects(id),
                    workflow_id TEXT REFERENCES workflows(id),
                    topology_id TEXT REFERENCES topologies(id),
                    parent_session_id TEXT REFERENCES sessions(id),
                    source_execution_id TEXT,
                    source_eval_id TEXT,
                    execution_mode TEXT NOT NULL DEFAULT 'fake' CHECK (
                        execution_mode IN ('fake', 'live_session')
                    ),
                    max_depth INTEGER NOT NULL CHECK (max_depth >= 0),
                    max_fanout INTEGER NOT NULL CHECK (max_fanout > 0),
                    max_descendants INTEGER NOT NULL CHECK (max_descendants > 0),
                    step_limit INTEGER NOT NULL CHECK (step_limit > 0),
                    last_stop_reason TEXT,
                    malformed_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    recovered_at TEXT,
                    quarantined_at TEXT,
                    quarantine_reason TEXT,
                    recovery_checked_at TEXT
                );
                INSERT INTO recursive_task_graphs_v65_new (
                    id, root_task_id, title, objective, status, project_id, workflow_id,
                    topology_id, parent_session_id, source_execution_id, source_eval_id,
                    execution_mode, max_depth, max_fanout, max_descendants, step_limit,
                    last_stop_reason, malformed_reason, created_at, updated_at, recovered_at,
                    quarantined_at, quarantine_reason, recovery_checked_at
                )
                SELECT
                    id, root_task_id, title, objective, status, project_id, workflow_id,
                    topology_id, parent_session_id, source_execution_id, source_eval_id,
                    execution_mode, max_depth, max_fanout, max_descendants, step_limit,
                    last_stop_reason, malformed_reason, created_at, updated_at, recovered_at,
                    quarantined_at, quarantine_reason, recovery_checked_at
                FROM recursive_task_graphs;
                DROP TABLE recursive_task_graphs;
                ALTER TABLE recursive_task_graphs_v65_new RENAME TO recursive_task_graphs;
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_graph_root
                    ON recursive_task_graphs(id, root_task_id);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_project
                    ON recursive_task_graphs(project_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_workflow
                    ON recursive_task_graphs(workflow_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_topology
                    ON recursive_task_graphs(topology_id, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_status
                    ON recursive_task_graphs(status, updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_graph_quarantine
                    ON recursive_task_graphs(quarantined_at, updated_at DESC)
                    WHERE quarantined_at IS NOT NULL;

                CREATE TABLE recursive_task_attempts_v65_new (
                    id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
                    retry_count INTEGER NOT NULL CHECK (retry_count >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'running', 'succeeded', 'decomposed', 'failed',
                        'blocked', 'cancelled', 'interrupted'
                    )),
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    failure_reason TEXT,
                    block_reason TEXT,
                    dependency_snapshot_json TEXT NOT NULL CHECK (json_valid(dependency_snapshot_json)),
                    executor_kind TEXT NOT NULL DEFAULT 'fake' CHECK (
                        executor_kind IN ('fake', 'live_session')
                    ),
                    session_id TEXT REFERENCES sessions(id),
                    workflow_execution_id TEXT,
                    CHECK (
                        (status = 'running' AND finished_at IS NULL)
                        OR (status <> 'running' AND finished_at IS NOT NULL)
                    ),
                    UNIQUE (task_id, phase, attempt_no)
                );
                INSERT INTO recursive_task_attempts_v65_new (
                    id, graph_id, task_id, phase, attempt_no, retry_count, status,
                    started_at, finished_at, failure_reason, block_reason,
                    dependency_snapshot_json, executor_kind, session_id, workflow_execution_id
                )
                SELECT
                    id, graph_id, task_id, phase, attempt_no, retry_count, status,
                    started_at, finished_at, failure_reason, block_reason,
                    dependency_snapshot_json, executor_kind, session_id, workflow_execution_id
                FROM recursive_task_attempts;
                DROP TABLE recursive_task_attempts;
                ALTER TABLE recursive_task_attempts_v65_new RENAME TO recursive_task_attempts;
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_task_phase
                    ON recursive_task_attempts(task_id, phase, attempt_no);
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_running
                    ON recursive_task_attempts(graph_id, status, started_at)
                    WHERE status = 'running';
                CREATE INDEX IF NOT EXISTS idx_recursive_attempts_session
                    ON recursive_task_attempts(session_id)
                    WHERE session_id IS NOT NULL;

                CREATE TABLE recursive_scheduler_runs_v65_new (
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
                    cancellation_request_id TEXT REFERENCES recursive_cancellation_requests(id),
                    cancellation_reason TEXT,
                    lease_owner TEXT,
                    lease_token TEXT,
                    lease_heartbeat_at TEXT,
                    lease_expires_at TEXT,
                    idempotency_key TEXT,
                    request_fingerprint TEXT,
                    policy_snapshot_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(policy_snapshot_json)),
                    CHECK (
                        (status IN ('running', 'cancelling') AND completed_at IS NULL)
                        OR (status NOT IN ('running', 'cancelling') AND completed_at IS NOT NULL)
                    )
                );
                INSERT INTO recursive_scheduler_runs_v65_new (
                    id, graph_id, status, source, operator, started_at, completed_at,
                    stop_reason, step_count, max_steps, executor_mode, failure_reason,
                    report_artifact_id, cancellation_request_id, cancellation_reason,
                    lease_owner, lease_token, lease_heartbeat_at, lease_expires_at,
                    idempotency_key, request_fingerprint, policy_snapshot_json
                )
                SELECT
                    id, graph_id, status, source, operator, started_at, completed_at,
                    stop_reason, step_count, max_steps, executor_mode, failure_reason,
                    report_artifact_id, cancellation_request_id, cancellation_reason,
                    lease_owner, lease_token, lease_heartbeat_at, lease_expires_at,
                    NULL, NULL, '{}'
                FROM recursive_scheduler_runs;
                DROP TABLE recursive_scheduler_runs;
                ALTER TABLE recursive_scheduler_runs_v65_new RENAME TO recursive_scheduler_runs;
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_graph_started
                    ON recursive_scheduler_runs(graph_id, started_at DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_status
                    ON recursive_scheduler_runs(status, started_at DESC);
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_one_active_per_graph
                    ON recursive_scheduler_runs(graph_id)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_active_lease
                    ON recursive_scheduler_runs(status, lease_expires_at, started_at)
                    WHERE status IN ('running', 'cancelling');
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_lease_owner
                    ON recursive_scheduler_runs(lease_owner, status, lease_expires_at)
                    WHERE lease_owner IS NOT NULL;
                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_scheduler_runs_live_idempotency
                    ON recursive_scheduler_runs(idempotency_key)
                    WHERE idempotency_key IS NOT NULL;

                COMMIT;
                PRAGMA foreign_keys=ON;",
            )?;
            tracing::info!("V65 migration complete: recursive DAG live scheduler store primitives");
            self.conn.pragma_update(None, "user_version", 65)?;
        }

        Ok(())
    }
}
