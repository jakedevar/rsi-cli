impl Store {
    fn migrate_v066(&self, version: i32) -> Result<()> {
        // V66: Recursive DAG typed inspector materialization foundation.
        //
        // These tables are store-only read-model foundations for future typed
        // inspector RPCs. The migration does not register routes or flip typed
        // inspector capabilities; write paths materialize daemon-owned rows in
        // their existing transactions.
        if version < 66 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_typed_artifact_roles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    artifact_id INTEGER NOT NULL REFERENCES recursive_execution_artifacts(id) ON DELETE CASCADE,
                    role TEXT NOT NULL CHECK (role IN (
                        'raw_output', 'normalized_output', 'validation_report',
                        'produced_artifact', 'test_summary', 'diff_summary',
                        'scheduler_report', 'topology_recursive_graph_creation'
                    )),
                    provenance TEXT NOT NULL CHECK (provenance IN (
                        'legacy', 'daemon_recorded', 'live_output_validation',
                        'scheduler_report', 'topology_creation'
                    )),
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    source_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    source_digest TEXT,
                    schema_version INTEGER CHECK (schema_version IS NULL OR schema_version >= 0),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (artifact_id, role, source)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_graph
                    ON recursive_typed_artifact_roles(graph_id, created_at DESC, id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_artifact
                    ON recursive_typed_artifact_roles(artifact_id, source);
                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_validation
                    ON recursive_typed_artifact_roles(validation_id, role)
                    WHERE validation_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_typed_artifact_roles_run
                    ON recursive_typed_artifact_roles(scheduler_run_id, role)
                    WHERE scheduler_run_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_test_results (
                    test_id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    test_index INTEGER NOT NULL DEFAULT 0 CHECK (test_index >= 0),
                    status TEXT NOT NULL CHECK (status IN (
                        'passed', 'failed', 'skipped', 'not_run', 'unknown'
                    )),
                    required INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
                    name TEXT,
                    command TEXT,
                    display_label TEXT NOT NULL CHECK (length(trim(display_label)) > 0),
                    duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
                    exit_code INTEGER,
                    failure_summary TEXT,
                    stdout_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    stderr_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    log_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    output_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(output_artifact_ids_json)),
                    related_validation_issue_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(related_validation_issue_ids_json)),
                    retry_decision_json TEXT CHECK (retry_decision_json IS NULL OR json_valid(retry_decision_json)),
                    suggested_next_action TEXT,
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    source_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    source_digest TEXT,
                    schema_version INTEGER CHECK (schema_version IS NULL OR schema_version >= 0),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_test_results_validation_index
                    ON recursive_test_results(validation_id, test_index)
                    WHERE validation_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_test_results_graph
                    ON recursive_test_results(graph_id, created_at DESC, test_id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_test_results_run
                    ON recursive_test_results(scheduler_run_id, created_at DESC, test_id DESC)
                    WHERE scheduler_run_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_test_results_artifact
                    ON recursive_test_results(artifact_id)
                    WHERE artifact_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_diff_summaries (
                    diff_id TEXT PRIMARY KEY,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    attempt_id TEXT REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    diff_index INTEGER NOT NULL DEFAULT 0 CHECK (diff_index >= 0),
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    file_count INTEGER NOT NULL CHECK (file_count >= 0),
                    binary_file_count INTEGER NOT NULL DEFAULT 0 CHECK (binary_file_count >= 0),
                    truncated_file_count INTEGER NOT NULL DEFAULT 0 CHECK (truncated_file_count >= 0),
                    additions INTEGER CHECK (additions IS NULL OR additions >= 0),
                    deletions INTEGER CHECK (deletions IS NULL OR deletions >= 0),
                    hunk_count INTEGER CHECK (hunk_count IS NULL OR hunk_count >= 0),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    source_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    source_digest TEXT,
                    schema_version INTEGER CHECK (schema_version IS NULL OR schema_version >= 0),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL
                );

                CREATE UNIQUE INDEX IF NOT EXISTS idx_recursive_diff_summaries_validation_index
                    ON recursive_diff_summaries(validation_id, diff_index)
                    WHERE validation_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_summaries_graph
                    ON recursive_diff_summaries(graph_id, created_at DESC, diff_id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_summaries_run
                    ON recursive_diff_summaries(scheduler_run_id, created_at DESC, diff_id DESC)
                    WHERE scheduler_run_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_summaries_artifact
                    ON recursive_diff_summaries(artifact_id)
                    WHERE artifact_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_diff_files (
                    file_id TEXT PRIMARY KEY,
                    diff_id TEXT NOT NULL REFERENCES recursive_diff_summaries(diff_id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    file_index INTEGER NOT NULL CHECK (file_index >= 0),
                    display_path TEXT NOT NULL CHECK (length(trim(display_path)) > 0),
                    previous_display_path TEXT,
                    status TEXT NOT NULL CHECK (status IN (
                        'added', 'modified', 'deleted', 'renamed',
                        'copied', 'unchanged', 'unknown'
                    )),
                    additions INTEGER CHECK (additions IS NULL OR additions >= 0),
                    deletions INTEGER CHECK (deletions IS NULL OR deletions >= 0),
                    hunk_count INTEGER CHECK (hunk_count IS NULL OR hunk_count >= 0),
                    binary INTEGER NOT NULL DEFAULT 0 CHECK (binary IN (0, 1)),
                    inside_allowed_root INTEGER CHECK (
                        inside_allowed_root IS NULL OR inside_allowed_root IN (0, 1)
                    ),
                    validation_issue_count INTEGER NOT NULL DEFAULT 0 CHECK (validation_issue_count >= 0),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (diff_id, file_index)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_diff_files_diff
                    ON recursive_diff_files(diff_id, file_index);
                CREATE INDEX IF NOT EXISTS idx_recursive_diff_files_graph
                    ON recursive_diff_files(graph_id, display_path);

                CREATE TABLE IF NOT EXISTS recursive_scheduler_reports (
                    report_id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    source TEXT NOT NULL CHECK (source IN (
                        'daemon_collected', 'live_validation', 'scheduler_report',
                        'legacy_backfill', 'model_claimed'
                    )),
                    trust_level TEXT NOT NULL CHECK (trust_level IN (
                        'verified', 'normalized', 'scheduler_owned',
                        'model_claimed', 'legacy_unverified'
                    )),
                    scheduler_source TEXT NOT NULL CHECK (scheduler_source IN (
                        'test_harness', 'manual_rpc', 'startup_recovery', 'future_daemon_loop'
                    )),
                    operator TEXT,
                    execution_mode TEXT NOT NULL CHECK (execution_mode IN ('fake', 'live_session')),
                    run_status TEXT NOT NULL CHECK (run_status IN (
                        'running', 'cancelling', 'completed', 'cancelled',
                        'failed', 'rejected', 'lease_expired'
                    )),
                    started_at TEXT NOT NULL,
                    completed_at TEXT,
                    stop_reason TEXT CHECK (
                        stop_reason IS NULL OR stop_reason IN (
                            'graph_terminal', 'idle_no_runnable', 'step_limit_exceeded',
                            'partial_failure', 'quarantined', 'cancellation_requested',
                            'lease_expired', 'executor_error', 'recovery_deferred'
                        )
                    ),
                    failure_reason TEXT,
                    step_count INTEGER NOT NULL CHECK (step_count >= 0),
                    max_steps INTEGER NOT NULL CHECK (max_steps > 0),
                    selected_task_count INTEGER NOT NULL DEFAULT 0 CHECK (selected_task_count >= 0),
                    live_attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (live_attempt_count >= 0),
                    validation_count INTEGER NOT NULL DEFAULT 0 CHECK (validation_count >= 0),
                    emitted_artifact_count INTEGER NOT NULL DEFAULT 0 CHECK (emitted_artifact_count >= 0),
                    cancellation_observed INTEGER NOT NULL DEFAULT 0 CHECK (cancellation_observed IN (0, 1)),
                    recovery_observed INTEGER NOT NULL DEFAULT 0 CHECK (recovery_observed IN (0, 1)),
                    report_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    UNIQUE (run_id)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_reports_graph
                    ON recursive_scheduler_reports(graph_id, started_at DESC, report_id DESC);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_reports_artifact
                    ON recursive_scheduler_reports(report_artifact_id)
                    WHERE report_artifact_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS recursive_scheduler_report_steps (
                    report_id TEXT NOT NULL REFERENCES recursive_scheduler_reports(report_id) ON DELETE CASCADE,
                    run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    step_index INTEGER NOT NULL CHECK (step_index >= 0),
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    phase TEXT NOT NULL CHECK (phase IN ('execute', 'integrate')),
                    attempt_id TEXT NOT NULL REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    live_attempt_id TEXT REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    validation_id TEXT REFERENCES recursive_live_output_validations(id) ON DELETE CASCADE,
                    outcome TEXT NOT NULL CHECK (outcome IN (
                        'succeeded', 'decomposed', 'failed',
                        'blocked', 'cancelled', 'stopped', 'unknown'
                    )),
                    message TEXT,
                    final_task_status TEXT NOT NULL CHECK (final_task_status IN (
                        'pending', 'planning', 'ready', 'running', 'decomposed',
                        'blocked_on_children', 'integrating', 'verifying',
                        'succeeded', 'failed', 'blocked', 'cancelled'
                    )),
                    emitted_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(emitted_artifact_ids_json)),
                    metadata_state TEXT NOT NULL CHECK (
                        metadata_state IN ('valid', 'malformed', 'legacy', 'unavailable')
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    created_at TEXT NOT NULL,
                    PRIMARY KEY (report_id, step_index)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_report_steps_run
                    ON recursive_scheduler_report_steps(run_id, step_index);
                CREATE INDEX IF NOT EXISTS idx_recursive_scheduler_report_steps_graph
                    ON recursive_scheduler_report_steps(graph_id, task_id, step_index);",
            )?;
            tracing::info!(
                "V66 migration complete: recursive DAG typed inspector materialization foundation"
            );
            self.conn.pragma_update(None, "user_version", 66)?;
        }

        Ok(())
    }
}
