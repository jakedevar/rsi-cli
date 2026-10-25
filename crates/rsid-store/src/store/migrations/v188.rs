// rsi-migration: additive
/// #149: the processes a source-worktree settlement run could not prove and so
/// never signalled. On macOS the kernel withholds the environment (and so the
/// RSI stamps) of a platform or hardened binary; settlement keeps refusing to
/// kill such a process, and this table puts it on the run's receipt so the
/// operator can stop it by hand. A row names the run root its working
/// directory lies in (session, custody and root together) or, with all three
/// NULL, the cohort repository. At most 64 rows per run. Rows are immutable and
/// never deleted (a BEFORE INSERT guard refuses `INSERT OR REPLACE` and UPSERT
/// collisions, the V179 pattern); a run recorded before V188 simply has none.
impl Store {
    fn migrate_v188(&self, version: i32) -> Result<()> {
        if version < 188 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE source_worktree_settlement_unproven_survivors (
                    run_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(run_id)) REFERENCES source_worktree_settlement_runs(run_id) ON DELETE RESTRICT,
                    sequence INTEGER NOT NULL CHECK(sequence BETWEEN 0 AND 63),
                    pid INTEGER NOT NULL CHECK(pid BETWEEN 1 AND 2147483647),
                    start_time_micros INTEGER NOT NULL CHECK(start_time_micros>=0),
                    executable TEXT CHECK(executable IS NULL OR octet_length(executable) BETWEEN 1 AND 16384),
                    session_id TEXT CHECK(session_id IS NULL OR rsi_uuid_is_canonical(session_id)),
                    custody_id TEXT CHECK(custody_id IS NULL OR rsi_uuid_is_canonical(custody_id)),
                    sandbox_root TEXT CHECK(sandbox_root IS NULL OR octet_length(sandbox_root) BETWEEN 1 AND 16384),
                    reason TEXT NOT NULL CHECK(octet_length(reason) BETWEEN 1 AND 64),
                    CHECK((session_id IS NULL AND custody_id IS NULL AND sandbox_root IS NULL)
                       OR (session_id IS NOT NULL AND custody_id IS NOT NULL AND sandbox_root IS NOT NULL)),
                    PRIMARY KEY(run_id,sequence)
                ) WITHOUT ROWID;
                CREATE TRIGGER source_worktree_settlement_unproven_survivors_no_update
                    BEFORE UPDATE ON source_worktree_settlement_unproven_survivors
                    BEGIN SELECT RAISE(ABORT,'settlement unproven survivors are immutable'); END;
                CREATE TRIGGER source_worktree_settlement_unproven_survivors_no_delete
                    BEFORE DELETE ON source_worktree_settlement_unproven_survivors
                    BEGIN SELECT RAISE(ABORT,'settlement unproven survivors are retained'); END;
                CREATE TRIGGER source_worktree_settlement_unproven_survivors_no_replace
                    BEFORE INSERT ON source_worktree_settlement_unproven_survivors
                    WHEN EXISTS(SELECT 1 FROM source_worktree_settlement_unproven_survivors
                        WHERE run_id=NEW.run_id AND sequence=NEW.sequence)
                    BEGIN SELECT RAISE(ABORT,'settlement unproven survivors are never replaced'); END;",
            )?;
            tx.pragma_update(None, "user_version", 188)?;
            tx.commit()?;
        }
        Ok(())
    }
}
