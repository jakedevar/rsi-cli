impl Store {
    fn migrate_v076(&self, version: i32) -> Result<()> {
        // V76: bounded D03 controller-tail lookup.
        if version < 76 {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(
                "
                CREATE INDEX IF NOT EXISTS idx_idea_events_controller_tail
                    ON idea_events(idea_id, sequence DESC)
                    WHERE event_type IN (
                        'controller_reserved',
                        'controller_assigned',
                        'controller_released'
                    );
                ",
            )?;
            tx.execute("PRAGMA user_version = 76", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
