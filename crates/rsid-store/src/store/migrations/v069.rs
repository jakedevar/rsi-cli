impl Store {
    fn migrate_v069(&self, version: i32) -> Result<()> {
        if version < 69 {
            // S10 / D6 content-staleness: graph_cache gains a content_hash column
            // so cache keys invalidate on the RESOLVED input CONTENT changing, not
            // just topology_version.
            //
            // Why ALTER (strict superset) rather than DROP+rebuild: graph_cache is
            // created via `CREATE TABLE IF NOT EXISTS` in
            // `graph_cache::init_cache_table` (not a numbered block), so it may not
            // exist yet on a given DB — hence the table_exists guard, mirroring V68.
            // Cache entries are disposable, so DROP+rebuild would be acceptable, but
            // a defaulted ADD COLUMN keeps pre-existing DBs readable and lets legacy
            // rows load with a blank content_hash instead of being silently dropped
            // (Reliability lens). The `TEXT NOT NULL DEFAULT ''` matches the fresh
            // schema in `init_cache_table`, so both creation paths converge.
            let table_exists: bool = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='graph_cache'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if table_exists {
                self.add_column_if_not_exists(
                    "graph_cache",
                    "content_hash",
                    "TEXT NOT NULL DEFAULT ''",
                )?;
            }
            tracing::info!("V69 migration complete: graph_cache.content_hash");
            self.conn.pragma_update(None, "user_version", 69)?;
        }

        Ok(())
    }
}
