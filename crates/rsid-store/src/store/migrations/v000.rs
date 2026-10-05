impl Store {
    fn migrate_v000(&self, _version: i32) -> Result<()> {
        // V0: Original schema
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id              TEXT PRIMARY KEY,
                claude_session_id TEXT,
                provider        TEXT NOT NULL DEFAULT 'Claude',
                query           TEXT NOT NULL,
                working_dir     TEXT NOT NULL,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS conversation_events (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id      TEXT NOT NULL REFERENCES sessions(id),
                sequence        INTEGER NOT NULL,
                event_type      TEXT NOT NULL,
                role            TEXT,
                content         TEXT NOT NULL DEFAULT '',
                tool_name       TEXT,
                tool_input      TEXT,
                created_at      TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_events_session_id
                ON conversation_events(session_id);
            CREATE INDEX IF NOT EXISTS idx_events_session_sequence
                ON conversation_events(session_id, sequence);

            CREATE TABLE IF NOT EXISTS approvals (
                id              TEXT PRIMARY KEY,
                session_id      TEXT NOT NULL REFERENCES sessions(id),
                tool_name       TEXT NOT NULL,
                tool_input      TEXT NOT NULL,
                status          TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                resolved_at     TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_approvals_session_id
                ON approvals(session_id);
            ",
        )?;

        Ok(())
    }
}
