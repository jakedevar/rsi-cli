impl Store {
    fn migrate_v124(&self, version: i32) -> Result<()> {
        // V124: live-scope manager record indexes so bookkeeping sweeps and
        // budget counts never range over archived history (Issue #643). The
        // literal must equal
        // harness_manager_v2::MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION.
        if version < 124 {
            harness_manager_v2::apply_live_bookkeeping_index_migration(self)?;
        }

        Ok(())
    }
}
