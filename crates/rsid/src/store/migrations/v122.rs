impl Store {
    fn migrate_v122(&self, version: i32) -> Result<()> {
        // V122: durable manager ledger facts keyed by the work, never the seat
        // (decision D19). Carries the newest row per identity forward; the
        // seat-scoped V2 records stay as historical input and in-flight state.
        if version < 122 {
            manager_ledger::apply_work_facts_migration(self)?;
        }

        Ok(())
    }
}
