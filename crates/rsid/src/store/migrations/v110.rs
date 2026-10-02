impl Store {
    fn migrate_v110(&self, version: i32) -> Result<()> {
        // V110: persist operator-selected Groups and project-wide scope. Legacy
        // appointments remain explicit, including empty/revoked appointments.
        if version < 110 {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "ALTER TABLE harness_manager_scopes ADD COLUMN scope_mode TEXT NOT NULL DEFAULT 'selected' CHECK(scope_mode IN ('selected','project'));
                 ALTER TABLE harness_manager_scopes ADD COLUMN group_ids_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(group_ids_json) AND json_type(group_ids_json)='array');
                 CREATE INDEX manager_project_scope_candidates ON sessions(project_id,session_kind,id);",
            )?;
            tx.execute("PRAGMA user_version = 110", [])?;
            tx.commit()?;
        }

        Ok(())
    }
}
