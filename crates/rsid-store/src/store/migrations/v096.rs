impl Store {
    fn migrate_v096(&self, version: i32) -> Result<()> {
        // V96: tool calls are asynchronous transactions, so their provider
        // correlation id is durable data rather than a display-only hint. Old
        // rows remain NULL: adjacency is not authoritative enough to backfill.
        if version < 96 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            add_column_if_not_exists_tx(&tx, "conversation_events", "tool_use_id", "TEXT")?;
            add_column_if_not_exists_tx(&tx, "conversation_events", "metadata", "TEXT")?;
            tx.execute(
                "CREATE INDEX IF NOT EXISTS idx_events_tool_use_id
                 ON conversation_events(tool_use_id)",
                [],
            )?;
            tx.execute("PRAGMA user_version = 96", [])?;
            tx.commit()?;
            tracing::info!("V96 migration complete: conversation event tool correlation");
        }

        Ok(())
    }
}
