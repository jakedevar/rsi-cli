impl Store {
    fn migrate_v037(&self, version: i32) -> Result<()> {
        // V37: Compiled prompts
        if version < 37 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS compiled_prompts (
                    id              TEXT PRIMARY KEY,
                    session_id      TEXT,
                    original_input  TEXT NOT NULL,
                    compiled_output TEXT NOT NULL,
                    contract_status TEXT NOT NULL,
                    layer_semantic  INTEGER NOT NULL DEFAULT 0,
                    layer_syntactic INTEGER NOT NULL DEFAULT 0,
                    layer_deictic   INTEGER NOT NULL DEFAULT 0,
                    layer_discourse INTEGER NOT NULL DEFAULT 0,
                    layer_pragmatic INTEGER NOT NULL DEFAULT 0,
                    accepted        INTEGER NOT NULL,
                    created_at      TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_compiled_prompts_session
                    ON compiled_prompts(session_id);
                CREATE INDEX IF NOT EXISTS idx_compiled_prompts_created
                    ON compiled_prompts(created_at DESC);",
            )?;
            tracing::info!("V37 migration complete: compiled_prompts table");
            self.conn.pragma_update(None, "user_version", 37)?;
        }

        Ok(())
    }
}
