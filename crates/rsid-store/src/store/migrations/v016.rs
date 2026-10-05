impl Store {
    fn migrate_v016(&self, version: i32) -> Result<()> {
        // V16: ESP games table
        if version < 16 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS esp_games (
                    id TEXT PRIMARY KEY,
                    played_at TEXT NOT NULL,
                    score INTEGER NOT NULL,
                    rounds_played INTEGER NOT NULL,
                    total_rounds INTEGER NOT NULL,
                    p_value REAL NOT NULL,
                    round_details TEXT NOT NULL
                );",
            )?;
            tracing::info!("V16 migration complete: esp_games table");
            self.conn.execute("PRAGMA user_version = 16", [])?;
        }

        Ok(())
    }
}
