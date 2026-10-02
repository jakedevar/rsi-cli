impl Store {
    fn migrate_v113(&self, version: i32) -> Result<()> {
        // V113: add the durable stable-identity archive success projection
        // after both V111 lineages have converged.
        if version < 113 {
            self.apply_archive_projection_v113_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v113-archive-projection-driver
    fn apply_archive_projection_v113_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 112 {
            return Err(DaemonError::Store(format!(
                "V113 requires exact V112 source, found V{active_version}"
            )));
        }
        archive_cleanup::validate_v102_catalog(&tx)?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterPreflight,
        )?;

        tx.execute_batch(
            "CREATE TABLE archive_cleanup_success_projections (
                projection_id TEXT PRIMARY KEY
                    CHECK(length(projection_id)=36 AND projection_id=lower(projection_id)),
                run_id TEXT NOT NULL UNIQUE
                    REFERENCES archive_cleanup_runs(run_id) ON DELETE RESTRICT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
                custody_id TEXT NOT NULL
                    REFERENCES sandbox_custody_roots(custody_id) ON DELETE RESTRICT,
                custody_generation INTEGER NOT NULL CHECK(custody_generation>0),
                projection_kind TEXT NOT NULL CHECK(projection_kind='session_archived'),
                schema_version INTEGER NOT NULL CHECK(schema_version=1),
                canonical_json TEXT NOT NULL CHECK(length(canonical_json)>2),
                projection_digest TEXT NOT NULL
                    CHECK(length(projection_digest)=71 AND projection_digest GLOB 'sha256:[0-9a-f]*'),
                receipt_digest TEXT NOT NULL
                    CHECK(length(receipt_digest)=71 AND receipt_digest GLOB 'sha256:[0-9a-f]*'),
                delivery_state TEXT NOT NULL CHECK(delivery_state IN ('pending','delivering','delivered')),
                row_version INTEGER NOT NULL CHECK(row_version>0),
                attempt_count INTEGER NOT NULL CHECK(attempt_count>=0),
                first_attempt_at TEXT,
                last_attempt_at TEXT,
                delivered_at TEXT,
                created_at TEXT NOT NULL CHECK(length(created_at) BETWEEN 20 AND 40),
                updated_at TEXT NOT NULL CHECK(length(updated_at) BETWEEN 20 AND 40),
                CHECK((delivery_state='pending' AND attempt_count=0 AND first_attempt_at IS NULL
                       AND last_attempt_at IS NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivering' AND attempt_count>0 AND first_attempt_at IS NOT NULL
                       AND last_attempt_at IS NOT NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivered' AND delivered_at IS NOT NULL))
            );
            CREATE TABLE archive_cleanup_projection_consumers (
                projection_id TEXT NOT NULL
                    REFERENCES archive_cleanup_success_projections(projection_id) ON DELETE RESTRICT,
                consumer_kind TEXT NOT NULL CHECK(consumer_kind IN ('bus','watch','memory')),
                delivery_state TEXT NOT NULL CHECK(delivery_state IN ('pending','delivering','delivered')),
                row_version INTEGER NOT NULL CHECK(row_version>0),
                attempt_count INTEGER NOT NULL CHECK(attempt_count>=0),
                first_attempt_at TEXT,
                last_attempt_at TEXT,
                delivered_at TEXT,
                created_at TEXT NOT NULL CHECK(length(created_at) BETWEEN 20 AND 40),
                updated_at TEXT NOT NULL CHECK(length(updated_at) BETWEEN 20 AND 40),
                PRIMARY KEY(projection_id,consumer_kind),
                CHECK((delivery_state='pending' AND attempt_count=0 AND first_attempt_at IS NULL
                       AND last_attempt_at IS NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivering' AND attempt_count>0 AND first_attempt_at IS NOT NULL
                       AND last_attempt_at IS NOT NULL AND delivered_at IS NULL)
                   OR (delivery_state='delivered' AND delivered_at IS NOT NULL))
            );",
        )?;
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::AfterTables)?;

        tx.execute_batch(
            "CREATE INDEX archive_cleanup_success_projections_pending
                 ON archive_cleanup_success_projections(updated_at,projection_id)
                 WHERE delivery_state!='delivered';
             CREATE INDEX archive_cleanup_projection_consumers_pending
                 ON archive_cleanup_projection_consumers(projection_id,consumer_kind)
                 WHERE delivery_state!='delivered';",
        )?;
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::AfterIndexes)?;

        tx.execute_batch(
            "CREATE TRIGGER archive_cleanup_success_projections_v103_association_insert
             BEFORE INSERT ON archive_cleanup_success_projections
             WHEN NOT EXISTS (
               SELECT 1 FROM archive_cleanup_runs run
               WHERE run.run_id=NEW.run_id AND run.phase='settled'
                 AND run.session_id=NEW.session_id AND run.custody_id=NEW.custody_id
                 AND run.custody_generation=NEW.custody_generation
                 AND run.receipt_digest=NEW.receipt_digest)
             BEGIN SELECT RAISE(ABORT,'V103 archive projection requires its settled run'); END;
             CREATE TRIGGER archive_cleanup_success_projections_v103_identity_immutable
             BEFORE UPDATE ON archive_cleanup_success_projections
             WHEN NEW.projection_id!=OLD.projection_id OR NEW.run_id!=OLD.run_id
               OR NEW.session_id!=OLD.session_id OR NEW.custody_id!=OLD.custody_id
               OR NEW.custody_generation!=OLD.custody_generation
               OR NEW.projection_kind!=OLD.projection_kind OR NEW.schema_version!=OLD.schema_version
               OR NEW.canonical_json!=OLD.canonical_json OR NEW.projection_digest!=OLD.projection_digest
               OR NEW.receipt_digest!=OLD.receipt_digest OR NEW.created_at!=OLD.created_at
             BEGIN SELECT RAISE(ABORT,'V103 archive projection identity is immutable'); END;
             CREATE TRIGGER archive_cleanup_success_projections_v103_state_forward
             BEFORE UPDATE ON archive_cleanup_success_projections
             WHEN NOT ((OLD.delivery_state='pending' AND NEW.delivery_state='delivering')
                    OR (OLD.delivery_state='delivering' AND NEW.delivery_state IN ('delivering','delivered')))
             BEGIN SELECT RAISE(ABORT,'V103 archive projection transition refused'); END;
             CREATE TRIGGER archive_cleanup_success_projections_v103_no_delete
             BEFORE DELETE ON archive_cleanup_success_projections
             BEGIN SELECT RAISE(ABORT,'V103 archive projections are immutable history'); END;
             CREATE TRIGGER archive_cleanup_projection_consumers_v103_forward
             BEFORE UPDATE ON archive_cleanup_projection_consumers
             WHEN NEW.projection_id!=OLD.projection_id OR NEW.consumer_kind!=OLD.consumer_kind
               OR NEW.created_at!=OLD.created_at
               OR NOT ((OLD.delivery_state='pending' AND NEW.delivery_state='delivering')
                    OR (OLD.delivery_state='delivering' AND NEW.delivery_state IN ('delivering','delivered')))
             BEGIN SELECT RAISE(ABORT,'V103 archive projection consumer transition refused'); END;
             CREATE TRIGGER archive_cleanup_projection_consumers_v103_no_delete
             BEFORE DELETE ON archive_cleanup_projection_consumers
             BEGIN SELECT RAISE(ABORT,'V103 archive projection consumers are immutable history'); END;",
        )?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterTriggers,
        )?;

        archive_cleanup::backfill_v103_settled_projections(&tx)?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterBackfill,
        )?;
        archive_cleanup::validate_v103_catalog(&tx)?;
        archive_cleanup::validate_v103_projection_rows(&tx)?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V113 archive projection migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V113 archive projection migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::AfterChecks)?;
        tx.execute("PRAGMA user_version = 113", [])?;
        archive_projection_v113_migration_fault(
            ArchiveProjectionV113MigrationFault::AfterUserVersion,
        )?;
        archive_projection_v113_migration_fault(ArchiveProjectionV113MigrationFault::BeforeCommit)?;
        tx.commit()?;
        tracing::info!("V113 migration complete: archive success projection installed");
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v113-archive-projection-driver
}
