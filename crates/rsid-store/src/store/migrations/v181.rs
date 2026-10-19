// rsi-migration: additive
/// #1671: a turn-custody row is never deleted or replaced through raw SQL. V161
/// guarded UPDATEs only, so a raw `DELETE` or `INSERT OR REPLACE` could still erase
/// a launch's identity (pid, start time, spool, worker scope) or its terminal
/// history, bypassing the retention the session purge enforces. A BEFORE DELETE
/// trigger refuses every delete. `INSERT OR REPLACE` deletes a conflicting row
/// without firing the DELETE trigger while `recursive_triggers` is off (the
/// V157/V165/V179 pattern), so a BEFORE INSERT guard refuses any insert that would
/// conflict with an existing row on the invocation, the spool directory, the
/// active-session slot or the implicit rowid (the table's primary key is TEXT, so
/// `rowid`/`_rowid_`/`oid` is a fourth unique key a REPLACE resolves by deleting).
/// `UPDATE OR REPLACE ... SET rowid=<other row>` deletes the same way without an
/// INSERT, so a BEFORE UPDATE guard refuses every rowid change. Existing rows are
/// untouched.
impl Store {
    fn migrate_v181(&self, version: i32) -> Result<()> {
        if version < 181 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "CREATE TRIGGER provider_turn_custody_no_delete
                    BEFORE DELETE ON provider_turn_custody
                    BEGIN SELECT RAISE(ABORT,'turn custody is append only'); END;
                CREATE TRIGGER provider_turn_custody_no_replace
                    BEFORE INSERT ON provider_turn_custody
                    WHEN EXISTS(SELECT 1 FROM provider_turn_custody
                        WHERE invocation_id=NEW.invocation_id
                           OR rowid=NEW.rowid
                           OR spool_dir=NEW.spool_dir
                           OR (session_id=NEW.session_id
                               AND state IN ('live','adopted')
                               AND NEW.state IN ('live','adopted')))
                    BEGIN SELECT RAISE(ABORT,'turn custody cannot be replaced'); END;
                CREATE TRIGGER provider_turn_custody_rowid_immutable
                    BEFORE UPDATE ON provider_turn_custody
                    WHEN NEW.rowid IS NOT OLD.rowid
                    BEGIN SELECT RAISE(ABORT,'turn custody rowid is immutable'); END;",
            )?;
            tx.pragma_update(None, "user_version", 181)?;
            tx.commit()?;
        }
        Ok(())
    }
}
