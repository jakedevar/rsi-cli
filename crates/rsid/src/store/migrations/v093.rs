impl Store {
    fn migrate_v093(&self, version: i32) -> Result<()> {
        if version < 93 {
            // Pioneer is a new stable SessionProvider string. V55 put a closed
            // provider CHECK on recursive_live_attempts, so existing databases
            // need a bounded rebuild before recursive live launches can persist
            // Pioneer without violating that historical constraint.
            self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let outcome = self.apply_pioneer_v93_migration();
            self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            outcome?;
        }

        {
            let tx = self.conn.unchecked_transaction()?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_requested_at",
                "TEXT",
            )?;
            add_column_if_not_exists_tx(&tx, "model_invocations", "cancellation_reason", "TEXT")?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_mechanism",
                "TEXT",
            )?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS model_budget_alert_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    invocation_id TEXT NOT NULL,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT NOT NULL,
                    metric TEXT NOT NULL,
                    remaining INTEGER NOT NULL,
                    limit_value INTEGER NOT NULL,
                    threshold INTEGER NOT NULL,
                    emitted_at TEXT NOT NULL,
                    UNIQUE(invocation_id, scope_kind, scope_id, purpose, metric, threshold)
                );
                CREATE INDEX IF NOT EXISTS idx_model_budget_alert_events_emitted_at
                    ON model_budget_alert_events(emitted_at DESC, id DESC);
                ",
            )?;
            tx.commit()?;
        }

        Ok(())
    }

    fn apply_pioneer_v93_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 92 {
            return Err(DaemonError::Store(format!(
                "V93 requires exact V92 source, found V{active_version}"
            )));
        }
        h1_v92_validate_catalog(&tx)?;

        tx.execute_batch(
            "CREATE TABLE recursive_live_attempts_v93_new (
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
                        'Claude', 'Codex', 'Pioneer', 'Local', 'Antigravity', 'Gemini', 'CodexAppServer', 'Harness'
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
            INSERT INTO recursive_live_attempts_v93_new (
                id, graph_id, task_id, scheduler_run_id, attempt_id, phase,
                attempt_no, session_id, provider, model, sandbox_kind, sandbox_root,
                sandbox_branch, sandbox_worktree_id, workflow_id, topology_id,
                workflow_execution_id, topology_workflow_id, execution_mode, status,
                recovery_status, prompt_artifact_id, output_artifact_id, diff_artifact_id,
                test_artifact_id, cancellation_request_id, lease_owner, lease_token,
                heartbeat_at, lease_expires_at, max_wall_time_ms, created_at, started_at,
                launched_at, completed_at, recovery_checked_at, recovered_at,
                failure_reason, interruption_reason, cancellation_reason, recovery_reason,
                error, updated_at
            )
            SELECT
                id, graph_id, task_id, scheduler_run_id, attempt_id, phase,
                attempt_no, session_id, provider, model, sandbox_kind, sandbox_root,
                sandbox_branch, sandbox_worktree_id, workflow_id, topology_id,
                workflow_execution_id, topology_workflow_id, execution_mode, status,
                recovery_status, prompt_artifact_id, output_artifact_id, diff_artifact_id,
                test_artifact_id, cancellation_request_id, lease_owner, lease_token,
                heartbeat_at, lease_expires_at, max_wall_time_ms, created_at, started_at,
                launched_at, completed_at, recovery_checked_at, recovered_at,
                failure_reason, interruption_reason, cancellation_reason, recovery_reason,
                error, updated_at
            FROM recursive_live_attempts;
            DROP TABLE recursive_live_attempts;
            ALTER TABLE recursive_live_attempts_v93_new RENAME TO recursive_live_attempts;
            CREATE INDEX idx_recursive_live_attempts_graph
                ON recursive_live_attempts(graph_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_task
                ON recursive_live_attempts(task_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_run
                ON recursive_live_attempts(scheduler_run_id, created_at DESC, id);
            CREATE INDEX idx_recursive_live_attempts_status
                ON recursive_live_attempts(status, updated_at DESC, id);
            CREATE UNIQUE INDEX idx_recursive_live_attempts_session
                ON recursive_live_attempts(session_id) WHERE session_id IS NOT NULL;
            CREATE INDEX idx_recursive_live_attempts_heartbeat_expiry
                ON recursive_live_attempts(status, lease_expires_at, heartbeat_at, id)
                WHERE lease_token IS NOT NULL
                  AND heartbeat_at IS NOT NULL
                  AND lease_expires_at IS NOT NULL;",
        )?;

        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V93 requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V93 Pioneer provider migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 93", [])?;
        tx.commit()?;
        tracing::info!("V93 migration complete: Pioneer recursive-live provider admission");
        Ok(())
    }
}
