impl Store {
    fn migrate_v090(&self, version: i32) -> Result<()> {
        if version < 90 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if active_version != 89 {
                return Err(DaemonError::Store(format!(
                    "V90 requires exact V89 source, found V{active_version}"
                )));
            }
            if !session_execution_projection_catalog_matches(&tx, false)?
                || !session_execution_projection_foreign_keys_match(&tx, false)?
            {
                return Err(DaemonError::Store(
                    "V90 requires exact V89 execution-projection catalog".into(),
                ));
            }
            let missing_projections: i64 = tx.query_row(
                "SELECT count(*) FROM sessions s
                 LEFT JOIN session_execution_projections p ON p.session_id=s.id
                 WHERE p.session_id IS NULL",
                [],
                |row| row.get(0),
            )?;
            if missing_projections != 0 {
                return Err(DaemonError::Store(format!(
                    "V90 requires exactly one execution projection per Session, found {missing_projections} missing"
                )));
            }
            let source_integrity: String =
                tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if source_integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V90 requires integrity_check=ok for exact V89 source, got {source_integrity}"
                )));
            }
            let source_foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if source_foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V90 requires clean V89 foreign keys, found {source_foreign_key_errors} violation(s)"
                )));
            }
            v90_migration_fault(V90MigrationFault::AfterPreflight)?;

            tx.execute_batch(
                "PRAGMA defer_foreign_keys=ON;
                 DROP TRIGGER sessions_execution_projection_after_insert;
                 DROP TRIGGER session_execution_projections_no_delete;
                 ALTER TABLE session_execution_projections
                     RENAME TO session_execution_projections_v89;",
            )?;
            v90_migration_fault(V90MigrationFault::AfterRename)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_V90_TABLE_SQL)?;
            v90_migration_fault(V90MigrationFault::AfterCreate)?;

            let source_count: i64 = tx.query_row(
                "SELECT count(*) FROM session_execution_projections_v89",
                [],
                |row| row.get(0),
            )?;
            tx.execute_batch(
                "INSERT INTO session_execution_projections
                    (session_id,schema_version,projection_version,execution_state,freshness,
                     canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                     validated_at,error_code,updated_at)
                 SELECT session_id,schema_version,projection_version,execution_state,freshness,
                        canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                        validated_at,error_code,updated_at
                 FROM session_execution_projections_v89;",
            )?;
            v90_corrupt_copy_for_test(&tx)?;
            let destination_count: i64 = tx.query_row(
                "SELECT count(*) FROM session_execution_projections",
                [],
                |row| row.get(0),
            )?;
            let source_minus_destination: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections_v89
                    EXCEPT
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections
                 )",
                [],
                |row| row.get(0),
            )?;
            let destination_minus_source: i64 = tx.query_row(
                "SELECT count(*) FROM (
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections
                    EXCEPT
                    SELECT session_id,schema_version,projection_version,execution_state,freshness,
                           canonical_repo_dir,effective_cwd,custody_id,custody_generation,
                           validated_at,error_code,updated_at
                    FROM session_execution_projections_v89
                 )",
                [],
                |row| row.get(0),
            )?;
            if source_count != destination_count
                || source_minus_destination != 0
                || destination_minus_source != 0
            {
                return Err(DaemonError::Store(format!(
                    "V90 execution-projection copy mismatch: source={source_count}, destination={destination_count}, source-minus-destination={source_minus_destination}, destination-minus-source={destination_minus_source}"
                )));
            }
            v90_migration_fault(V90MigrationFault::AfterCopy)?;

            tx.execute_batch("DROP TABLE session_execution_projections_v89;")?;
            v90_migration_fault(V90MigrationFault::AfterOldTableDrop)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_INDEX_SQL)?;
            v90_migration_fault(V90MigrationFault::AfterIndexes)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_NO_DELETE_SQL)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_AFTER_INSERT_SQL)?;
            tx.execute_batch(SESSION_EXECUTION_PROJECTION_PURGE_GUARD_SQL)?;
            v90_migration_fault(V90MigrationFault::AfterTriggers)?;

            if !session_execution_projection_catalog_matches(&tx, true)?
                || !session_execution_projection_foreign_keys_match(&tx, true)?
            {
                return Err(DaemonError::Store(
                    "V90 execution-projection result catalog mismatch".into(),
                ));
            }
            let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            if integrity != "ok" {
                return Err(DaemonError::Store(format!(
                    "V90 requires integrity_check=ok, got {integrity}"
                )));
            }
            let foreign_key_errors: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })?;
            if foreign_key_errors != 0 {
                return Err(DaemonError::Store(format!(
                    "V90 execution-projection foreign-key check found {foreign_key_errors} violation(s)"
                )));
            }
            v90_migration_fault(V90MigrationFault::AfterChecks)?;
            tx.execute("PRAGMA user_version = 90", [])?;
            v90_migration_fault(V90MigrationFault::AfterUserVersion)?;
            v90_migration_fault(V90MigrationFault::BeforeCommit)?;
            tx.commit()?;
            tracing::info!(
                "V90 migration complete: detached retained execution projections with guarded purge"
            );
        }

        Ok(())
    }
}
