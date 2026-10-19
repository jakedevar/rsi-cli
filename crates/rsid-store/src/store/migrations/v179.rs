// rsi-migration: additive
/// #2635: the roots a source-worktree settlement run retained, kept on the run
/// record. A run journals only its eligible roots (`source_worktree_settlement_items`)
/// and counts the rest, so "what did this pass keep and why" had to be rebuilt
/// from the audit and re-classified by hand. One row per retained root, written
/// in the same transaction as the run's intent, carries the root's identity, its
/// retention class (`disposition`) and the audit's diagnostic. Rows are
/// immutable and never deleted (a BEFORE INSERT guard refuses `INSERT OR REPLACE`
/// and UPSERT collisions, which would otherwise delete a row without firing the
/// DELETE trigger while `recursive_triggers` is off, the V157/V165 pattern); a run recorded before V179 simply has none.
impl Store {
    fn migrate_v179(&self, version: i32) -> Result<()> {
        if version < 179 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE source_worktree_settlement_retained_roots (
                    run_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(run_id)) REFERENCES source_worktree_settlement_runs(run_id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK(sequence BETWEEN 0 AND 255),
                    session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(session_id)),
                    custody_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(custody_id)),
                    session_status TEXT NOT NULL CHECK(octet_length(session_status)<=64),
                    sandbox_root TEXT NOT NULL CHECK(octet_length(sandbox_root) BETWEEN 1 AND 16384),
                    source_ref TEXT NOT NULL CHECK(octet_length(source_ref)<=16384),
                    disposition TEXT NOT NULL CHECK(octet_length(disposition) BETWEEN 1 AND 64 AND disposition!='eligible_integrated_ancestor'),
                    diagnostic TEXT CHECK(diagnostic IS NULL OR octet_length(diagnostic)<=16384),
                    PRIMARY KEY(run_id,sequence)
                ) WITHOUT ROWID;
                CREATE TRIGGER source_worktree_settlement_retained_roots_no_update
                    BEFORE UPDATE ON source_worktree_settlement_retained_roots
                    BEGIN SELECT RAISE(ABORT,'settlement retained roots are immutable'); END;
                CREATE TRIGGER source_worktree_settlement_retained_roots_no_delete
                    BEFORE DELETE ON source_worktree_settlement_retained_roots
                    BEGIN SELECT RAISE(ABORT,'settlement retained roots are retained'); END;
                CREATE TRIGGER source_worktree_settlement_retained_roots_no_replace
                    BEFORE INSERT ON source_worktree_settlement_retained_roots
                    WHEN EXISTS(SELECT 1 FROM source_worktree_settlement_retained_roots
                        WHERE run_id=NEW.run_id AND sequence=NEW.sequence)
                    BEGIN SELECT RAISE(ABORT,'settlement retained roots are never replaced'); END;",
            )?;
            tx.pragma_update(None, "user_version", 179)?;
            tx.commit()?;
        }
        Ok(())
    }
}
