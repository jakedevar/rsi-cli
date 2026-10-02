impl Store {
    fn migrate_v034(&self, version: i32) -> Result<()> {
        // V34: Lifecycle hooks, tool permissions, and context compression
        if version < 34 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS permission_rules (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    tool_pattern TEXT NOT NULL,
                    action TEXT NOT NULL DEFAULT 'Ask',
                    scope TEXT NOT NULL DEFAULT 'global',
                    scope_id TEXT,
                    priority INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_perm_rules_scope
                    ON permission_rules(scope, scope_id);

                CREATE TABLE IF NOT EXISTS offloaded_content (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    event_sequence INTEGER NOT NULL,
                    content_hash TEXT NOT NULL,
                    original_content TEXT NOT NULL,
                    byte_size INTEGER NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(session_id, event_sequence)
                );
                CREATE INDEX IF NOT EXISTS idx_offload_session_seq
                    ON offloaded_content(session_id, event_sequence);",
            )?;
            tracing::info!("V34 migration complete: permission_rules + offloaded_content tables");
            self.conn.execute("PRAGMA user_version = 34", [])?;
        }

        Ok(())
    }
}
