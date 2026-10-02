impl Store {
    fn migrate_v060(&self, version: i32) -> Result<()> {
        // V60: Recursive DAG live output validation persistence.
        //
        // Phase 6.5C records validator decisions and artifact links so later
        // read-only status RPCs can inspect live output commits without
        // scanning execution artifacts or reinterpreting raw model JSON.
        if version < 60 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS recursive_live_output_validations (
                    id TEXT PRIMARY KEY,
                    live_attempt_id TEXT NOT NULL REFERENCES recursive_live_attempts(id) ON DELETE CASCADE,
                    graph_id TEXT NOT NULL REFERENCES recursive_task_graphs(id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL REFERENCES recursive_task_nodes(id) ON DELETE CASCADE,
                    scheduler_run_id TEXT NOT NULL REFERENCES recursive_scheduler_runs(id) ON DELETE CASCADE,
                    attempt_id TEXT NOT NULL REFERENCES recursive_task_attempts(id) ON DELETE CASCADE,
                    session_id TEXT REFERENCES sessions(id),
                    raw_output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    normalized_output_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    validation_artifact_id INTEGER REFERENCES recursive_execution_artifacts(id),
                    status TEXT NOT NULL CHECK (status IN (
                        'valid', 'invalid', 'repairable', 'ambiguous',
                        'operator_review_required'
                    )),
                    output_kind TEXT CHECK (
                        output_kind IS NULL OR output_kind IN (
                            'success', 'decomposition', 'retryable_failure',
                            'permanent_failure', 'blocked', 'cancelled'
                        )
                    ),
                    mapping_decision TEXT CHECK (
                        mapping_decision IS NULL OR mapping_decision IN (
                            'success', 'decomposition', 'retry_failure',
                            'permanent_failure', 'blocked', 'cancelled',
                            'repair_same_live_attempt', 'fail_attempt',
                            'block_task', 'operator_review', 'no_op'
                        )
                    ),
                    issue_count INTEGER NOT NULL CHECK (issue_count >= 0),
                    error_count INTEGER NOT NULL CHECK (error_count >= 0),
                    warning_count INTEGER NOT NULL CHECK (warning_count >= 0),
                    info_count INTEGER NOT NULL CHECK (info_count >= 0),
                    issue_summary_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(issue_summary_json)),
                    parser_source_json TEXT CHECK (
                        parser_source_json IS NULL OR json_valid(parser_source_json)
                    ),
                    retry_decision_json TEXT CHECK (
                        retry_decision_json IS NULL OR json_valid(retry_decision_json)
                    ),
                    produced_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(produced_artifact_ids_json)),
                    test_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(test_artifact_ids_json)),
                    diff_artifact_ids_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(diff_artifact_ids_json)),
                    validation_report_json TEXT CHECK (
                        validation_report_json IS NULL OR json_valid(validation_report_json)
                    ),
                    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
                    normalized_digest TEXT,
                    created_at TEXT NOT NULL,
                    UNIQUE (live_attempt_id, normalized_digest)
                );

                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_live
                    ON recursive_live_output_validations(live_attempt_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_attempt
                    ON recursive_live_output_validations(attempt_id, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_session
                    ON recursive_live_output_validations(session_id, created_at DESC, id)
                    WHERE session_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_status
                    ON recursive_live_output_validations(status, created_at DESC, id);
                CREATE INDEX IF NOT EXISTS idx_recursive_live_output_validations_digest
                    ON recursive_live_output_validations(live_attempt_id, normalized_digest)
                    WHERE normalized_digest IS NOT NULL;",
            )?;
            tracing::info!("V60 migration complete: recursive DAG live output validations");
            self.conn.pragma_update(None, "user_version", 60)?;
        }

        Ok(())
    }
}
