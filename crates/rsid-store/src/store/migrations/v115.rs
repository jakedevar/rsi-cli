impl Store {
    fn migrate_v115(&self, version: i32) -> Result<()> {
        // V114 -> V115 precondition. Every migration from V115 onward
        // authenticates its source with an exact full-sqlite_master
        // fingerprint, which hashes each object's stored SQL text. A database
        // deployed before `sessions.provider` and `workflows.definition_json`
        // were folded into their base CREATE TABLE carries them as trailing
        // ALTER TABLE columns: same schema, different text, different
        // fingerprint. Rebuild those two tables onto the canonical catalog here
        // so the pinned V115/V116 drivers authenticate the database instead of
        // refusing it. No-op when the catalog is already canonical.
        if version <= 114 {
            self.converge_v114_deployed_additive_catalog()?;
        }

        // V115: retained, authority-bound prepared manager actions. Transport
        // and admission APIs are introduced separately from this journal.
        if version < 115 {
            self.apply_manager_prepared_actions_v115_migration()?;
        }

        Ok(())
    }
}
