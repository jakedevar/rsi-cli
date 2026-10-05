impl Store {
    fn migrate_v048(&self, version: i32) -> Result<()> {
        // V48: Daemon settings key-value table (RSI-026).
        // Authoritative store for global daemon-owned settings that previously
        // lived in ~/.rsi/state.json. First inhabitant: system_prompt_preset.
        // Future single-value daemon settings reuse this table by string key.
        if version < 48 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS daemon_settings (
                    key        TEXT PRIMARY KEY,
                    value      TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );",
            )?;
            tracing::info!("V48 migration complete: daemon_settings key-value table (RSI-026)");
            self.conn.pragma_update(None, "user_version", 48)?;
        }

        Ok(())
    }
}
