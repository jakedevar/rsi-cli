// rsi-migration: contract
// Starter names coexist with private manager/Epic definitions. Ownership is
// the durable scope (project and Epic), not the rotating writer session.
// Rebuild only to remove V43's global name UNIQUE; retain every row and rowid.
impl Store {
    fn migrate_v182(&self, version: i32) -> Result<()> {
        if version < 182 {
            // Do not rename the old table: inbound foreign keys must continue
            // to name topologies, and DROP must not run ON DELETE actions.
            self.conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
            let outcome = (|| -> Result<()> {
                let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
                tx.execute_batch(
                    "CREATE TABLE topologies_rebuilt (
                        id TEXT PRIMARY KEY,
                        name TEXT NOT NULL,
                        definition_json TEXT NOT NULL,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        owner_kind TEXT NOT NULL DEFAULT 'operator' CHECK(owner_kind IN ('operator','manager','epic')),
                        owner_session_id TEXT,
                        project_id TEXT,
                        epic_id TEXT,
                        revision INTEGER NOT NULL DEFAULT 1 CHECK(revision > 0),
                        definition_digest TEXT,
                        shared INTEGER NOT NULL DEFAULT 0 CHECK(shared IN (0,1)),
                        archived_at TEXT
                    );
                    INSERT INTO topologies_rebuilt (rowid,id,name,definition_json,created_at,updated_at,
                        owner_kind,owner_session_id,project_id,epic_id,revision,definition_digest,shared,archived_at)
                    SELECT rowid,id,name,definition_json,created_at,updated_at,
                        owner_kind,owner_session_id,project_id,epic_id,revision,definition_digest,shared,archived_at
                    FROM topologies;
                    DROP TABLE topologies;
                    ALTER TABLE topologies_rebuilt RENAME TO topologies;
                    CREATE INDEX idx_topologies_name ON topologies(name);
                    CREATE UNIQUE INDEX idx_topologies_owner_name ON topologies(
                        name,owner_kind,COALESCE(project_id,''),COALESCE(epic_id,''));",
                )?;
                tx.pragma_update(None, "user_version", 182)?;
                tx.commit()?;
                Ok(())
            })();
            self.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            outcome?;
        }
        Ok(())
    }
}
