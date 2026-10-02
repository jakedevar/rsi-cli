impl Store {
    fn migrate_v074(&self, version: i32) -> Result<()> {
        if version < 74 {
            let tx = self.conn.unchecked_transaction()?;
            add_column_if_not_exists_tx(
                &tx,
                "model_budget_policies",
                "alert_threshold_ratio",
                "REAL",
            )?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_requested_at",
                "TEXT",
            )?;
            add_column_if_not_exists_tx(&tx, "model_invocations", "cancellation_reason", "TEXT")?;
            add_column_if_not_exists_tx(
                &tx,
                "model_invocations",
                "cancellation_mechanism",
                "TEXT",
            )?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS model_budget_alert_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    invocation_id TEXT NOT NULL,
                    scope_kind TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    purpose TEXT NOT NULL,
                    metric TEXT NOT NULL,
                    remaining INTEGER NOT NULL,
                    limit_value INTEGER NOT NULL,
                    threshold INTEGER NOT NULL,
                    emitted_at TEXT NOT NULL,
                    UNIQUE(invocation_id, scope_kind, scope_id, purpose, metric, threshold)
                );
                CREATE INDEX IF NOT EXISTS idx_model_budget_alert_events_emitted_at
                    ON model_budget_alert_events(emitted_at DESC, id DESC);
                ",
            )?;
            tx.execute("PRAGMA user_version = 74", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
