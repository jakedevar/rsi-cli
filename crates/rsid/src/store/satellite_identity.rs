//! Local installation identity. The value is generated once in V135 and is
//! deliberately independent of daemon process and session identities.

use super::Store;
use crate::error::{DaemonError, Result};
use rusqlite::{Transaction, TransactionBehavior, params};
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: v135-satellite-identity-migration
pub(crate) fn apply_v135_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 134 {
        return Err(DaemonError::Store(format!(
            "V135 requires V134, found V{version}"
        )));
    }
    tx.execute_batch(
        "CREATE TABLE satellite_installation_identity (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            installation_id TEXT NOT NULL CHECK (length(installation_id) = 36)
        );",
    )?;
    tx.execute(
        "INSERT INTO satellite_installation_identity (singleton, installation_id) VALUES (1, ?1)",
        params![Uuid::new_v4().to_string()],
    )?;
    tx.execute("PRAGMA user_version = 135", [])?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v135-satellite-identity-migration

impl Store {
    pub(crate) fn satellite_installation_id(&self) -> Result<Uuid> {
        let value: String = self.conn.query_row(
            "SELECT installation_id FROM satellite_installation_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let id = Uuid::parse_str(&value)
            .map_err(|_| DaemonError::Store("invalid satellite installation identity".into()))?;
        if id.is_nil() || id.to_string() != value {
            return Err(DaemonError::Store(
                "invalid satellite installation identity".into(),
            ));
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn installation_identity_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("satellite.db");
        let first = Store::open(&path).unwrap();
        let installation_id = first.satellite_installation_id().unwrap();
        assert!(!installation_id.is_nil());
        let version: i32 = first
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, super::super::LATEST_SCHEMA_VERSION);
        drop(first);
        let second = Store::open(&path).unwrap();
        assert_eq!(second.satellite_installation_id().unwrap(), installation_id);
    }
}
