impl Store {
    fn migrate_v043(&self, version: i32) -> Result<()> {
        // V43: Multi-tag system + DB-stored named topology templates.
        //   - sessions.tag: fallback single-tag column for legacy/migration compat
        //     (multi-tag lives in session_tags; this column carries the canonical
        //     "primary" tag for legacy single-tag callsites).
        //   - session_tags: join table for the multi-tag rollout (P1.5 owns inserts).
        //   - topologies: DB-stored named templates (P1.4 owns inserts/CRUD).
        // All DDL is idempotent (CREATE ... IF NOT EXISTS / pragma_table_info gate
        // in add_column_if_not_exists). Foreign-key clauses are documentation-grade
        // until a future ticket enables PRAGMA foreign_keys = ON.
        if version < 43 {
            self.add_column_if_not_exists("sessions", "tag", "TEXT NOT NULL DEFAULT ''")?;
            self.conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_tag ON sessions(tag)",
                [],
            )?;

            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_tags (
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    tag        TEXT NOT NULL,
                    PRIMARY KEY (session_id, tag)
                );

                CREATE INDEX IF NOT EXISTS idx_session_tags_tag ON session_tags(tag);

                CREATE TABLE IF NOT EXISTS topologies (
                    id              TEXT PRIMARY KEY,
                    name            TEXT NOT NULL UNIQUE,
                    definition_json TEXT NOT NULL,
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_topologies_name ON topologies(name);",
            )?;

            tracing::info!(
                "V43 migration complete: sessions.tag column + idx_sessions_tag, \
                 session_tags table + index, topologies table + index"
            );
            self.conn.pragma_update(None, "user_version", 43)?;
        }

        Ok(())
    }
}
