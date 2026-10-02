impl Store {
    fn migrate_v142(&self, version: i32) -> Result<()> {
        // #929: durable operator message queue with soft interrupt. The version
        // is provisional; the lander assigns the final number.
        if version < 142 {
            operator_messages::apply_migration(self, 142)?;
        }

        Ok(())
    }
}
