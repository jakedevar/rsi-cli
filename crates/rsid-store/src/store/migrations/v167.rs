/// Durable, immutable attribution for all new source-worktree intents (#1879).
impl Store {
    fn migrate_v167(&self, version: i32) -> Result<()> {
        if version < 167 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TABLE source_worktree_settlement_attribution (
                    run_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(run_id)) REFERENCES source_worktree_settlement_runs(run_id) ON DELETE RESTRICT,
                    origin TEXT NOT NULL CHECK(origin IN ('operator','agent','automatic')),
                    agent_session_id TEXT CHECK(agent_session_id IS NULL OR rsi_uuid_is_canonical(agent_session_id)),
                    CHECK((origin='agent') = (agent_session_id IS NOT NULL))
                );
                CREATE TRIGGER source_worktree_settlement_attribution_no_replace
                    BEFORE INSERT ON source_worktree_settlement_attribution
                    WHEN EXISTS(SELECT 1 FROM source_worktree_settlement_attribution WHERE run_id=NEW.run_id)
                    BEGIN SELECT RAISE(ABORT,'settlement attribution is never replaced'); END;
                CREATE TRIGGER source_worktree_settlement_attribution_no_update
                    BEFORE UPDATE ON source_worktree_settlement_attribution
                    BEGIN SELECT RAISE(ABORT,'settlement attribution is immutable'); END;
                CREATE TRIGGER source_worktree_settlement_attribution_no_delete
                    BEFORE DELETE ON source_worktree_settlement_attribution
                    BEGIN SELECT RAISE(ABORT,'settlement attribution is retained'); END;",
            )?;
            tx.pragma_update(None, "user_version", 167)?;
            tx.commit()?;
        }
        Ok(())
    }
}
