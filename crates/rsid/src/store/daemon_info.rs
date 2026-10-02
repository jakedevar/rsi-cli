//! `AgentGetDaemonInfo` (#1045): the database's schema version, read live.

use super::Store;
use crate::error::Result;

impl Store {
    /// `PRAGMA user_version` of the open database.
    ///
    /// # Errors
    /// A persistence error when the pragma cannot be read.
    pub(crate) fn schema_user_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?)
    }
}
