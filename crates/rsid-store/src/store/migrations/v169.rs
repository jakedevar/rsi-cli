// rsi-migration: additive
/// #2012 #2013 #2014: scope authority belongs to the durable launch record.
impl Store {
    fn migrate_v169(&self, version: i32) -> Result<()> {
        if version < 169 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "ALTER TABLE provider_turn_custody ADD COLUMN worker_scope TEXT
                    CHECK(worker_scope IS NULL OR (
                        worker_scope GLOB 'rsi-worker-*.scope'
                        AND length(worker_scope)<=255
                        AND worker_scope NOT GLOB '*[^a-zA-Z0-9_.-]*'));
                CREATE TRIGGER provider_turn_custody_scope_immutable
                    BEFORE UPDATE OF worker_scope ON provider_turn_custody
                    WHEN NEW.worker_scope IS NOT OLD.worker_scope
                    BEGIN SELECT RAISE(ABORT,'turn custody scope is immutable'); END;",
            )?;
            tx.pragma_update(None, "user_version", 169)?;
            tx.commit()?;
        }
        Ok(())
    }
}
