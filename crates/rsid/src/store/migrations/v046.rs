impl Store {
    fn migrate_v046(&self, version: i32) -> Result<()> {
        // V46: Master-Improve chain iteration metadata (one row per iteration).
        // (Originally written as V45 in PR #10; renumbered during 2026-05-15 merge
        // because PR #18 took V45 for capability_class.)
        if version < 46 {
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS chain_iterations (
                    chain_id            TEXT NOT NULL,
                    iteration_index     INTEGER NOT NULL,
                    parent_execution_id TEXT,
                    child_execution_id  TEXT NOT NULL,
                    halt_reason         TEXT,
                    goal_text           TEXT NOT NULL,
                    refined_goal_text   TEXT,
                    token_count         INTEGER,
                    pre_failure_count   INTEGER,
                    post_failure_count  INTEGER,
                    cap                 INTEGER NOT NULL,
                    started_at          TEXT NOT NULL,
                    ended_at            TEXT,
                    PRIMARY KEY (chain_id, iteration_index)
                )",
                [],
            )?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_chain_iterations_child_exec
                    ON chain_iterations(child_execution_id)",
                [],
            )?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_chain_iterations_chain_active
                    ON chain_iterations(chain_id, halt_reason)
                    WHERE halt_reason IS NULL",
                [],
            )?;
            tracing::info!("V46 migration complete: chain_iterations table");
            self.conn.pragma_update(None, "user_version", 46)?;
        }

        Ok(())
    }
}
