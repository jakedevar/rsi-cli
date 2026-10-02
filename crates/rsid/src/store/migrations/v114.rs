impl Store {
    fn migrate_v114(&self, version: i32) -> Result<()> {
        // V114: durable, receipt-validated detachment receipts for the Epic
        // lineages the V111 branch normalization re-rooted, so manager
        // authority can still prove the original attribution from a
        // constrained relation rather than from an erased column.
        if version < 114 {
            self.apply_lineage_detachment_v114_migration()?;
        }

        Ok(())
    }

    // RSI-RELEASED-MIGRATION-BEGIN: v114-lineage-detachment-driver
    /// V114: durable, receipt-validated detachment receipts for the Epic
    /// lineages the V111 branch normalization re-rooted.
    ///
    /// The records are reconstructed from the receipts themselves, never from
    /// a hard-coded identity list: a session whose `continued_from` is NULL yet
    /// which a committed reservation or a manager rotation edge names as a
    /// successor can only exist because something nulled the column, and the
    /// normalization is the only writer in the tree that does. Both proofs are
    /// required: the receipt proves the edge, and the `rotation_events` journal
    /// row proves this tree performed the detachment. A disagreement or a
    /// missing journal row aborts, so a column nulled by anything else never
    /// earns a receipt.
    fn apply_lineage_detachment_v114_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 113 {
            return Err(DaemonError::Store(format!(
                "V114 requires exact V113 source, found V{active_version}"
            )));
        }
        lineage_detachment_v114_migration_fault(
            LineageDetachmentV114MigrationFault::AfterPreflight,
        )?;

        tx.execute_batch(
            "CREATE TABLE session_lineage_detachments (
                session_id TEXT PRIMARY KEY
                    REFERENCES sessions(id) ON DELETE RESTRICT
                    CHECK(length(session_id)=36 AND session_id=lower(session_id)),
                detached_from_session_id TEXT NOT NULL
                    REFERENCES sessions(id) ON DELETE RESTRICT
                    CHECK(length(detached_from_session_id)=36
                          AND detached_from_session_id=lower(detached_from_session_id)),
                receipt_class TEXT NOT NULL
                    CHECK(receipt_class IN ('harness_manager_rotation_edge','committed_reservation')),
                normalization_schema TEXT NOT NULL
                    CHECK(normalization_schema='v111-branch-normalization/1'),
                recorded_at TEXT NOT NULL CHECK(length(recorded_at) BETWEEN 20 AND 40),
                CHECK(session_id <> detached_from_session_id)
            );
            CREATE INDEX session_lineage_detachments_predecessor
                ON session_lineage_detachments(detached_from_session_id, session_id);
            CREATE TRIGGER session_lineage_detachments_v114_validate_insert
            BEFORE INSERT ON session_lineage_detachments
            WHEN NOT (
                (NEW.receipt_class='harness_manager_rotation_edge' AND EXISTS(
                    SELECT 1 FROM harness_manager_rotation_edges
                     WHERE predecessor_session_id=NEW.detached_from_session_id
                       AND successor_session_id=NEW.session_id))
             OR (NEW.receipt_class='committed_reservation' AND EXISTS(
                    SELECT 1 FROM agent_successor_reservations
                     WHERE predecessor_session_id=NEW.detached_from_session_id
                       AND candidate_session_id=NEW.session_id
                       AND state='committed'))
            ) BEGIN SELECT RAISE(ABORT,'V114 lineage detachment requires a matching receipt'); END;
            CREATE TRIGGER session_lineage_detachments_v114_no_update
            BEFORE UPDATE ON session_lineage_detachments
            BEGIN SELECT RAISE(ABORT,'V114 lineage detachment receipts are immutable'); END;
            CREATE TRIGGER session_lineage_detachments_v114_no_delete
            BEFORE DELETE ON session_lineage_detachments
            BEGIN SELECT RAISE(ABORT,'V114 lineage detachment receipts are retained'); END;",
        )?;
        lineage_convergence::validate_v114_catalog(&tx)?;
        lineage_detachment_v114_migration_fault(LineageDetachmentV114MigrationFault::AfterCatalog)?;

        let inserted = lineage_convergence::reconstruct_v114_detachment_rows(&tx)?;
        let foreign_key_errors: i64 = tx.query_row(
            "SELECT count(*) FROM pragma_foreign_key_check('session_lineage_detachments')",
            [],
            |row| row.get(0),
        )?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V114 lineage detachment migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        lineage_detachment_v114_migration_fault(LineageDetachmentV114MigrationFault::AfterRows)?;

        tx.execute("PRAGMA user_version = 114", [])?;
        tx.commit()?;
        tracing::info!(
            reconstructed = inserted,
            "V114 migration complete: lineage detachment receipts installed"
        );
        Ok(())
    }
    // RSI-RELEASED-MIGRATION-END: v114-lineage-detachment-driver
}
