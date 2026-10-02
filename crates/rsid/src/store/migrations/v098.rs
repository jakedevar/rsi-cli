impl Store {
    fn migrate_v098(&self, version: i32) -> Result<()> {
        // V98: restored sandbox sessions receive a fresh immutable allocation
        // identity, so their recreated worktree cannot collide with a purged
        // historical custody root for the same Session id.
        if version < 98 {
            self.apply_sandbox_allocation_identity_v98_migration()?;
        }

        Ok(())
    }

    /// Preserve a unique filesystem allocation identity for each custody root.
    /// Historical roots are immutable, so restoring a purged session must be
    /// able to allocate a distinct path while retaining the same Session id.
    fn apply_sandbox_allocation_identity_v98_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if active_version != 97 {
            return Err(DaemonError::Store(format!(
                "V98 requires exact V97 source, found V{active_version}"
            )));
        }
        add_column_if_not_exists_tx(&tx, "sandbox_custody_roots", "allocation_id", "TEXT")?;
        tx.execute(
            "UPDATE sandbox_custody_roots
             SET allocation_id=(
                 SELECT e.to_owner_session_id
                 FROM sandbox_custody_events e
                 WHERE e.custody_id=sandbox_custody_roots.custody_id
                   AND e.sequence=1
                   AND e.event_kind='allocated'
             )
             WHERE allocation_id IS NULL",
            [],
        )?;
        let missing: i64 = tx.query_row(
            "SELECT count(*) FROM sandbox_custody_roots
             WHERE allocation_id IS NULL OR length(trim(allocation_id))=0",
            [],
            |row| row.get(0),
        )?;
        if missing != 0 {
            return Err(DaemonError::Store(format!(
                "V98 allocation identity migration found {missing} root(s) without an allocation id"
            )));
        }
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_sandbox_custody_roots_allocation_id
                 ON sandbox_custody_roots(allocation_id);",
        )?;
        let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(DaemonError::Store(format!(
                "V98 allocation identity migration requires integrity_check=ok, got {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(DaemonError::Store(format!(
                "V98 allocation identity migration found {foreign_key_errors} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 98", [])?;
        tx.commit()?;
        tracing::info!("V98 migration complete: sandbox allocation identities");
        Ok(())
    }
}
