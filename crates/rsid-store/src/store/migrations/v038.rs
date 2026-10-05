impl Store {
    fn migrate_v038(&self, version: i32) -> Result<()> {
        // V38: Session rating + harness versioning + outcome proxies
        if version < 38 {
            self.add_column_if_not_exists("sessions", "rating", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "harness_version_hash", "TEXT")?;
            self.add_column_if_not_exists("sessions", "test_passed", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "clippy_passed", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "turn_count", "INTEGER")?;
            self.add_column_if_not_exists("sessions", "retry_count", "INTEGER")?;
            tracing::info!(
                "V38 migration complete: session rating, harness versioning, outcome proxies"
            );
            self.conn.pragma_update(None, "user_version", 38)?;
        }

        Ok(())
    }
}
