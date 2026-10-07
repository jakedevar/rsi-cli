//! Consistent pre-migration snapshots and offline rollback. Never restore while
//! a daemon owns the database: the supervisor invokes recovery after reaping it.
use super::LATEST_SCHEMA_VERSION;
use crate::error::{DaemonError, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct MigrationBackup {
    pub path: PathBuf,
    pub from_version: i32,
    pub to_version: i32,
    #[serde(default)]
    pub failed_path: Option<PathBuf>,
}

pub fn marker_path(database: &Path) -> PathBuf {
    suffix(database, ".migration-backup.json")
}

fn suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub fn read_marker(database: &Path) -> Result<Option<MigrationBackup>> {
    match std::fs::read(marker_path(database)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_marker(database: &Path, marker: &MigrationBackup) -> Result<()> {
    let path = marker_path(database);
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(marker)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn version(database: &Path) -> Result<i32> {
    let conn = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Snapshot before the first migration. VACUUM INTO includes committed WAL
/// pages, unlike copying rsi.db, and accepts a bound filename (no SQL quoting).
pub(super) fn before_migration(conn: &Connection, database: &Path) -> Result<()> {
    let from_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if from_version >= LATEST_SCHEMA_VERSION {
        return Ok(());
    }
    let result = (|| -> Result<()> {
        let directory = database.parent().unwrap_or(Path::new(".")).join("backups");
        std::fs::create_dir_all(&directory)?;
        // Disk is tight. Remove only the exact prior automatic snapshot named
        // by our marker; a failed replacement still refuses every migration.
        if let Some(previous) = read_marker(database)? {
            if previous.failed_path.is_some() {
                return Err(DaemonError::Store(
                    "offline database recovery is pending".into(),
                ));
            }
            // A migration can fail after committing some intermediate steps.
            // On retry retain the original predecessor, rather than backing up
            // the partial upgrade and losing compatibility with the old binary.
            if previous.to_version == LATEST_SCHEMA_VERSION
                && previous.from_version <= from_version
                && previous.path.exists()
                && version(&previous.path)? == previous.from_version
            {
                tracing::info!(backup = %previous.path.display(), "reusing pre-migration database backup for migration retry");
                return Ok(());
            }
            if previous.path.parent() == Some(directory.as_path())
                && previous.path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("rsi-pre-v") && name.ends_with(".db")
                })
            {
                match std::fs::remove_file(&previous.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
        let path = directory.join(format!("rsi-pre-v{from_version}-{stamp}.db"));
        let filename = path
            .to_str()
            .ok_or_else(|| DaemonError::Store("backup path is not UTF-8".into()))?;
        tracing::info!(backup = %path.display(), from_version, "creating pre-migration database backup");
        if let Err(error) = conn.execute("VACUUM INTO ?1", [filename]) {
            // A failed VACUUM may leave a partial output. It is never a backup.
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        std::fs::File::open(&path)?.sync_all()?;
        let marker = MigrationBackup {
            path,
            from_version,
            to_version: LATEST_SCHEMA_VERSION,
            failed_path: None,
        };
        if let Err(error) = write_marker(database, &marker) {
            let _ = std::fs::remove_file(&marker.path);
            return Err(error);
        }
        tracing::info!(backup = %marker.path.display(), from_version, to_version = LATEST_SCHEMA_VERSION, "pre-migration database backup ready");
        Ok(())
    })();
    result.map_err(|error| {
        DaemonError::Store(format!(
            "pre-migration backup failed; refusing unprotected migration: {error}"
        ))
    })
}

/// A stale marker alone is insufficient: read the actual database version.
pub fn needs_restore(database_version: i32, supported: i32, marker: &MigrationBackup) -> bool {
    database_version > supported
        && marker.from_version <= supported
        && marker.to_version > supported
        && database_version <= marker.to_version
}

/// Caller must hold the daemon instance lease and have no open Store. Preserve
/// the failed database and both sidecars. Record the destination first so a
/// killed recovery can resume, including between the database and WAL renames.
pub fn restore(database: &Path, supported: i32) -> Result<Option<PathBuf>> {
    let Some(mut marker) = read_marker(database)? else {
        if database.exists() && version(database)? > supported {
            return Err(DaemonError::Store(
                "newer database has no pre-migration backup; refusing binary rollback".into(),
            ));
        }
        return Ok(None);
    };
    if marker.failed_path.is_none() && !needs_restore(version(database)?, supported, &marker) {
        if version(database)? > supported {
            return Err(DaemonError::Store(
                "pre-migration backup does not cover binary rollback".into(),
            ));
        }
        return Ok(None);
    }
    // A prior recovery may have consumed the backup just before being killed.
    if marker.failed_path.is_some()
        && !marker.path.exists()
        && database.exists()
        && version(database)? <= supported
    {
        marker.failed_path = None;
        write_marker(database, &marker)?;
        return Ok(Some(marker.path));
    }
    if version(&marker.path)? != marker.from_version || marker.from_version > supported {
        return Err(DaemonError::Store(
            "pre-migration backup schema is incompatible".into(),
        ));
    }
    if marker.failed_path.is_none() {
        marker.failed_path = Some(suffix(
            database,
            &format!(
                ".failed-{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ")
            ),
        ));
        write_marker(database, &marker)?;
    }
    let failed = marker.failed_path.as_ref().unwrap();
    for ending in ["", "-wal", "-shm"] {
        let source = suffix(database, ending);
        let destination = suffix(failed, ending);
        if source.exists() {
            if destination.exists() {
                return Err(DaemonError::Store(format!(
                    "recovery destination already exists: {}",
                    destination.display()
                )));
            }
            std::fs::rename(source, destination)?;
        }
    }
    // Consume the snapshot by rename: restoring a 6 GB hub must not need a
    // third database-sized allocation. The failed database is retained.
    std::fs::rename(&marker.path, database)?;
    tracing::warn!(backup = %marker.path.display(), failed = %failed.display(), "restored pre-migration database for binary rollback");
    // The restored database version makes repeated requests harmless; never
    // prune failed databases.
    marker.failed_path = None;
    write_marker(database, &marker)?;
    Ok(Some(marker.path))
}

#[cfg(test)]
mod tests {
    use super::super::Store;
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_older_open_snapshots_before_migrating_and_current_open_skips() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        let predecessor = Store::open_test_predecessor_v84(&database).unwrap();
        predecessor
            .set_daemon_setting("backup_witness", "committed WAL data")
            .unwrap();
        let store = Store::open(&database).unwrap();
        let marker = read_marker(&database).unwrap().unwrap();
        assert_eq!(marker.from_version, 84);
        assert_eq!(marker.to_version, LATEST_SCHEMA_VERSION);
        assert_eq!(version(&marker.path).unwrap(), 84);
        let backup = Connection::open(&marker.path).unwrap();
        assert_eq!(
            backup
                .query_row(
                    "SELECT value FROM daemon_settings WHERE key = 'backup_witness'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "committed WAL data"
        );
        drop(backup);
        drop(predecessor);
        drop(store);
        let bytes = std::fs::read(marker_path(&database)).unwrap();
        Store::open(&database).unwrap();
        assert_eq!(std::fs::read(marker_path(&database)).unwrap(), bytes);
        assert_eq!(
            std::fs::read_dir(temp.path().join("backups"))
                .unwrap()
                .count(),
            1
        );
        assert!(restore(&database, LATEST_SCHEMA_VERSION).unwrap().is_none());
        assert_eq!(restore(&database, 84).unwrap(), Some(marker.path.clone()));
        assert_eq!(version(&database).unwrap(), 84);
        assert!(restore(&database, 84).unwrap().is_none());
        assert!(std::fs::read_dir(temp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("rsi.db.failed-")
        }));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_failure_blocks_migration() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        drop(Store::open_test_predecessor_v84(&database).unwrap());
        std::fs::write(temp.path().join("backups"), b"blocked directory").unwrap();
        let error = Store::open(&database).err().unwrap().to_string();
        assert!(error.contains("refusing unprotected migration"), "{error}");
        assert_eq!(version(&database).unwrap(), 84);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_interrupted_restore_preserves_remaining_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        let backup = temp.path().join("backup.db");
        let failed = suffix(&database, ".failed-test");
        Connection::open(&backup)
            .unwrap()
            .pragma_update(None, "user_version", 10)
            .unwrap();
        std::fs::write(&failed, b"preserved failed database").unwrap();
        std::fs::write(suffix(&database, "-wal"), b"preserved WAL").unwrap();
        std::fs::write(suffix(&database, "-shm"), b"preserved SHM").unwrap();
        write_marker(
            &database,
            &MigrationBackup {
                path: backup.clone(),
                from_version: 10,
                to_version: 12,
                failed_path: Some(failed.clone()),
            },
        )
        .unwrap();
        assert_eq!(restore(&database, 10).unwrap(), Some(backup));
        assert_eq!(version(&database).unwrap(), 10);
        assert_eq!(
            std::fs::read(suffix(&failed, "-wal")).unwrap(),
            b"preserved WAL"
        );
        assert_eq!(
            std::fs::read(suffix(&failed, "-shm")).unwrap(),
            b"preserved SHM"
        );
        assert_eq!(std::fs::read(failed).unwrap(), b"preserved failed database");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_partial_migration_retry_keeps_original_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        let conn = Connection::open(&database).unwrap();
        conn.pragma_update(None, "user_version", 10).unwrap();
        before_migration(&conn, &database).unwrap();
        let original = read_marker(&database).unwrap().unwrap();
        conn.pragma_update(None, "user_version", 11).unwrap();
        before_migration(&conn, &database).unwrap();
        let retry = read_marker(&database).unwrap().unwrap();
        assert_eq!(retry.path, original.path);
        assert_eq!(version(&retry.path).unwrap(), 10);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_restore_decision_checks_live_and_backup_versions() {
        let marker = MigrationBackup {
            path: "backup.db".into(),
            from_version: 10,
            to_version: 12,
            failed_path: None,
        };
        assert!(needs_restore(12, 10, &marker));
        assert!(!needs_restore(10, 10, &marker));
        assert!(!needs_restore(12, 12, &marker));
        assert!(!needs_restore(13, 10, &marker));
        assert!(!needs_restore(12, 9, &marker));
    }
}
