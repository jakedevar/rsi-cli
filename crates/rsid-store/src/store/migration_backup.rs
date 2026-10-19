//! Consistent pre-migration snapshots and offline rollback. Never restore while
//! a daemon owns the database: the supervisor invokes recovery after reaping it.
use super::LATEST_SCHEMA_VERSION;
use crate::error::{DaemonError, Result};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Pages copied per online-backup step; measured (#2372) as the fastest of the
/// step sizes tried, with no sleep between steps.
const BACKUP_PAGES_PER_STEP: std::ffi::c_int = 4096;
/// A source that stays busy or locked this long fails the backup, matching the
/// bounded wait `VACUUM INTO` gets from the connection's busy timeout.
const BACKUP_BUSY_LIMIT: Duration = Duration::from_secs(10);
const BACKUP_BUSY_RETRY: Duration = Duration::from_millis(10);

/// How the snapshot file was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotMethod {
    /// `rusqlite::backup`: a straight page copy, used when few pages are free.
    OnlineBackup,
    /// `VACUUM INTO`: also drops free pages from the copy.
    VacuumInto,
}

impl SnapshotMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::OnlineBackup => "online_backup",
            Self::VacuumInto => "vacuum_into",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SnapshotReport {
    method: SnapshotMethod,
    page_count: u64,
    freelist_count: u64,
}

/// The page copy is used only while the freelist is strictly below 5% of the
/// database pages (`freelist / pages < 1/20`, as `freelist * 20 < pages`: exact
/// integer arithmetic, saturating so it cannot overflow). Exactly 5%, an empty
/// database (zero pages) and anything freer go to `VACUUM INTO`, which drops the
/// free pages from the snapshot.
fn free_pages_allow_online_backup(freelist_count: u64, page_count: u64) -> bool {
    freelist_count.saturating_mul(20) < page_count
}

fn pragma_count(conn: &Connection, pragma: &str) -> Result<u64> {
    let value: i64 = conn.query_row(&format!("PRAGMA main.{pragma}"), [], |row| row.get(0))?;
    Ok(u64::try_from(value).unwrap_or(0))
}

/// Create the empty snapshot file, owner-only and never over an existing one.
fn create_new_private(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

/// A failed copy may leave a partial output and its sidecars. Never a backup.
fn remove_partial_snapshot(path: &Path) {
    for ending in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(suffix(path, ending));
    }
}

fn copy_in_steps(backup: &Backup<'_, '_>, pages_per_step: std::ffi::c_int) -> Result<()> {
    let mut busy_since: Option<Instant> = None;
    loop {
        match backup.step(pages_per_step)? {
            StepResult::Done => return Ok(()),
            StepResult::More => busy_since = None,
            _ => {
                let since = *busy_since.get_or_insert_with(Instant::now);
                if since.elapsed() > BACKUP_BUSY_LIMIT {
                    return Err(DaemonError::Store(
                        "online backup source stayed busy".into(),
                    ));
                }
                std::thread::sleep(BACKUP_BUSY_RETRY);
            }
        }
    }
}

fn online_backup(conn: &Connection, path: &Path, pages_per_step: std::ffi::c_int) -> Result<()> {
    let mut destination = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    {
        let backup = Backup::new(conn, &mut destination)?;
        copy_in_steps(&backup, pages_per_step)?;
    }
    // The page copy carries the source's WAL file-format bytes. Leave the
    // snapshot rollback-format with no sidecars, as `VACUUM INTO` does.
    let _mode: String =
        destination.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    destination.close().map_err(|(_, error)| error)?;
    Ok(())
}

/// Write a consistent snapshot of `conn` (committed WAL pages included) to the
/// not-yet-existing `path`. A failure removes everything it created.
fn write_snapshot(conn: &Connection, path: &Path) -> Result<SnapshotReport> {
    write_snapshot_with(conn, path, None, BACKUP_PAGES_PER_STEP)
}

/// [`write_snapshot`] with the method forced (`Some`) or chosen from the free
/// pages (`None`), and the page-copy step size given. A step of `-1` copies the
/// whole database in one step: one read transaction on the source, so a busy
/// WAL database cannot restart it (the online backup restarts after any write
/// by another connection between steps).
fn write_snapshot_with(
    conn: &Connection,
    path: &Path,
    forced: Option<SnapshotMethod>,
    pages_per_step: std::ffi::c_int,
) -> Result<SnapshotReport> {
    let filename = path
        .to_str()
        .ok_or_else(|| DaemonError::Store("backup path is not UTF-8".into()))?;
    let page_count = pragma_count(conn, "page_count")?;
    let freelist_count = pragma_count(conn, "freelist_count")?;
    let method = forced.unwrap_or_else(|| {
        if free_pages_allow_online_backup(freelist_count, page_count) {
            SnapshotMethod::OnlineBackup
        } else {
            SnapshotMethod::VacuumInto
        }
    });
    // Fails if the file exists, so an earlier snapshot is never overwritten. An
    // empty output file is valid for both `VACUUM INTO` and the page copy.
    create_new_private(path)?;
    let copied = match method {
        SnapshotMethod::OnlineBackup => online_backup(conn, path, pages_per_step),
        SnapshotMethod::VacuumInto => conn
            .execute("VACUUM INTO ?1", [filename])
            .map(drop)
            .map_err(Into::into),
    };
    if let Err(error) = copied {
        remove_partial_snapshot(path);
        return Err(error);
    }
    Ok(SnapshotReport {
        method,
        page_count,
        freelist_count,
    })
}

/// How an operator-requested snapshot copies the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseSnapshotMode {
    /// One-step page copy (one read transaction; carries the free pages).
    PageCopy,
    /// `VACUUM INTO`: the control a cold-archive measurement compares against.
    VacuumInto,
}

/// What [`snapshot_database`] produced.
#[derive(Debug, Clone)]
pub struct DatabaseSnapshot {
    pub method: &'static str,
    pub page_count: u64,
    pub freelist_count: u64,
    pub bytes: u64,
    /// Wall time of the copy itself (opening the source and registering the
    /// SQL functions are excluded).
    pub elapsed: Duration,
}

/// Take a consistent copy of the database at `source` into the not-yet-existing
/// `out`, for an agent or operator, without writing next to `source`.
///
/// The source is opened read-only with the schema's SQL functions registered
/// (the CHECK constraints call them, so a plain `sqlite3 ... VACUUM INTO` fails
/// with `no such function`), and the copy is a single read transaction, so a
/// live daemon's writes cannot restart it (#2530). Safe against a running daemon.
pub fn snapshot_database(
    source: &Path,
    out: &Path,
    mode: DatabaseSnapshotMode,
) -> Result<DatabaseSnapshot> {
    if !source.is_file() {
        return Err(DaemonError::Store(format!(
            "source database {} does not exist",
            source.display()
        )));
    }
    let conn = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BACKUP_BUSY_LIMIT)?;
    super::Store::register_sql_functions(&conn)?;
    let method = match mode {
        DatabaseSnapshotMode::PageCopy => SnapshotMethod::OnlineBackup,
        DatabaseSnapshotMode::VacuumInto => SnapshotMethod::VacuumInto,
    };
    let started = Instant::now();
    let report = write_snapshot_with(&conn, out, Some(method), -1)?;
    let elapsed = started.elapsed();
    Ok(DatabaseSnapshot {
        method: report.method.as_str(),
        page_count: report.page_count,
        freelist_count: report.freelist_count,
        bytes: std::fs::metadata(out)?.len(),
        elapsed,
    })
}

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

/// Snapshot before the first migration. Both snapshot methods include committed
/// WAL pages, unlike copying rsi.db: the online page copy when the freelist is
/// below 5% of the pages, `VACUUM INTO` (which also drops free pages) otherwise.
pub(super) fn before_migration(conn: &Connection, database: &Path) -> Result<()> {
    let from_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // SQLite reports an empty filename for an in-memory main database. Such a
    // connection has no recovery target, so creating a marker or snapshot
    // would only leave artifacts relative to the process working directory.
    let database_file: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
    if database_file.is_empty() || from_version >= LATEST_SCHEMA_VERSION {
        return Ok(());
    }
    let result = (|| -> Result<()> {
        let directory = database.parent().unwrap_or(Path::new(".")).join("backups");
        rsid_core::private_state::create_private_dir_all(&directory)?;
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
        // The random tail keeps two databases migrating in one directory at the
        // same clock tick (processes sharing a temp dir) from colliding on the
        // exclusive create below.
        let unique = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let path = directory.join(format!("rsi-pre-v{from_version}-{stamp}-{unique}.db"));
        tracing::info!(backup = %path.display(), from_version, "creating pre-migration database backup");
        let started = Instant::now();
        let report = write_snapshot(conn, &path)?;
        std::fs::File::open(&path)?.sync_all()?;
        let backup_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let bytes = std::fs::metadata(&path).map(|meta| meta.len()).ok();
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
        tracing::info!(
            backup = %marker.path.display(),
            from_version,
            to_version = LATEST_SCHEMA_VERSION,
            method = report.method.as_str(),
            page_count = report.page_count,
            freelist_count = report.freelist_count,
            bytes,
            backup_ms,
            "pre-migration database backup ready"
        );
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

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// #1844: a fresh database, its WAL sidecars and the migration backup are
    /// owner-only; opening a pre-existing 0644 database tightens it.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn store_open_makes_database_files_owner_only_fresh_and_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let fresh = temp.path().join("fresh.db");
        let store = Store::open(&fresh).unwrap();
        store.set_daemon_setting("witness", "wal row").unwrap();
        assert_eq!(mode_of(&fresh), 0o600);
        for sidecar in ["fresh.db-wal", "fresh.db-shm"] {
            let path = temp.path().join(sidecar);
            assert!(path.exists(), "{sidecar} exists while the store is open");
            assert_eq!(mode_of(&path), 0o600, "{sidecar}");
        }
        drop(store);

        let database = temp.path().join("rsi.db");
        drop(Store::open_test_predecessor_v84(&database).unwrap());
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _store = Store::open(&database).unwrap();
        assert_eq!(mode_of(&database), 0o600);
        let backups = temp.path().join("backups");
        assert_eq!(mode_of(&backups), 0o700);
        let backup = read_marker(&database).unwrap().unwrap().path;
        assert_eq!(mode_of(&backup), 0o600);
        assert_eq!(mode_of(&marker_path(&database)), 0o600);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn opening_memory_store_does_not_create_backup_files_in_crate_directory() {
        use std::collections::BTreeSet;

        fn backup_artifacts(directory: &Path) -> BTreeSet<PathBuf> {
            let mut artifacts = BTreeSet::new();
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".migration-backup.json") {
                    artifacts.insert(entry.path());
                }
                if name == "backups" && entry.file_type().unwrap().is_dir() {
                    for backup in std::fs::read_dir(entry.path()).unwrap() {
                        let backup = backup.unwrap();
                        if backup
                            .file_name()
                            .to_string_lossy()
                            .starts_with("rsi-pre-v")
                        {
                            artifacts.insert(backup.path());
                        }
                    }
                }
            }
            artifacts
        }

        let crate_directory = Path::new(rsi_common::manifest_dir!());
        let before = backup_artifacts(crate_directory);
        let stray = Path::new(":memory:");
        let stray_before = stray.exists();
        let store = Store::open(Path::new(":memory:")).unwrap();
        drop(store);
        let after = backup_artifacts(crate_directory);
        // No stray `:memory:` file either: it dirties a sandbox worktree.
        assert!(
            stray_before || !stray.exists(),
            "Store::open(\":memory:\") created a file in the working directory"
        );

        assert_eq!(
            after, before,
            "in-memory Store::open added backup artifacts"
        );
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

    /// A live WAL database whose CHECK constraint calls a daemon-registered SQL
    /// function, like the production schema. The connection stays open, holding
    /// the committed rows in the WAL.
    fn checked_database(path: &Path, rows: u32) -> Connection {
        let conn = Connection::open(path).unwrap();
        Store::register_sql_functions(&conn).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "CREATE TABLE rows (
                 id TEXT PRIMARY KEY CHECK (rsi_uuid_is_canonical(id) = 1),
                 payload BLOB NOT NULL
             );",
        )
        .unwrap();
        for n in 0..rows {
            conn.execute(
                "INSERT INTO rows (id, payload) VALUES (?1, zeroblob(3000))",
                [format!("00000000-0000-4000-8000-{n:012}")],
            )
            .unwrap();
        }
        conn
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn snapshot_database_copies_a_function_checked_wal_database_both_ways_2530() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        let live = checked_database(&database, 300);
        // The premise: a connection without the functions cannot VACUUM INTO.
        let plain =
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let plain_out = temp.path().join("plain.db");
        assert!(
            plain
                .execute("VACUUM INTO ?1", [plain_out.to_str().unwrap()])
                .is_err()
        );
        drop(plain);
        let before = file_names(temp.path());
        for (mode, method) in [
            (DatabaseSnapshotMode::PageCopy, "online_backup"),
            (DatabaseSnapshotMode::VacuumInto, "vacuum_into"),
        ] {
            let out = temp.path().join(format!("{method}.db"));
            let report = snapshot_database(&database, &out, mode).unwrap();
            assert_eq!(report.method, method);
            assert!(report.page_count > 0);
            assert_eq!(report.bytes, std::fs::metadata(&out).unwrap().len());
            assert_eq!(row_count(&out), 300);
        }
        // Only the two outputs were added: the source's directory gained no
        // other file, and the live connection is still usable.
        let mut expected = before.clone();
        expected.extend(["online_backup.db".to_string(), "vacuum_into.db".to_string()]);
        expected.sort();
        assert_eq!(file_names(temp.path()), expected);
        live.execute("DELETE FROM rows", []).unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn snapshot_database_never_overwrites_and_never_creates_a_missing_source_2530() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        let _live = checked_database(&database, 3);
        let existing = temp.path().join("existing.db");
        std::fs::write(&existing, b"keep").unwrap();
        assert!(snapshot_database(&database, &existing, DatabaseSnapshotMode::PageCopy).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");

        let missing = temp.path().join("missing.db");
        let out = temp.path().join("out.db");
        assert!(snapshot_database(&missing, &out, DatabaseSnapshotMode::VacuumInto).is_err());
        assert!(!missing.exists());
        assert!(!out.exists());
    }

    /// A WAL-mode database of `rows` one-page rows, `user_version` 10, with its
    /// committed pages still in the WAL (the connection stays open).
    fn rows_database(path: &Path, rows: u32) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "user_version", 10).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE rows (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
             WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {rows})
             INSERT INTO rows SELECT x, zeroblob(3000) FROM c;"
        ))
        .unwrap();
        conn
    }

    fn row_count(path: &Path) -> i64 {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM rows", [], |row| row.get(0))
            .unwrap()
    }

    fn file_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_online_copy_threshold_is_strictly_below_five_percent_free() {
        // Below 5%: page copy.
        assert!(free_pages_allow_online_backup(0, 1));
        assert!(free_pages_allow_online_backup(4, 100));
        assert!(free_pages_allow_online_backup(49, 1000));
        assert!(free_pages_allow_online_backup(339_999, 6_800_000));
        // Exactly 5% and above: VACUUM INTO.
        assert!(!free_pages_allow_online_backup(5, 100));
        assert!(!free_pages_allow_online_backup(1, 20));
        assert!(!free_pages_allow_online_backup(50, 1000));
        assert!(!free_pages_allow_online_backup(340_000, 6_800_000));
        assert!(!free_pages_allow_online_backup(99, 100));
        // An empty database and a count that would overflow never take the page copy.
        assert!(!free_pages_allow_online_backup(0, 0));
        assert!(!free_pages_allow_online_backup(u64::MAX, u64::MAX));
        assert!(!free_pages_allow_online_backup(u64::MAX / 10, u64::MAX));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_low_freelist_uses_multi_step_online_copy_of_wal_data() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        // More than one 4096-page step.
        let conn = rows_database(&source, 6000);
        assert!(pragma_count(&conn, "page_count").unwrap() > 4096);
        let snapshot = temp.path().join("snapshot.db");
        let report = write_snapshot(&conn, &snapshot).unwrap();
        assert_eq!(report.method, SnapshotMethod::OnlineBackup);
        assert_eq!(report.freelist_count, 0);
        assert_eq!(version(&snapshot).unwrap(), 10);
        assert_eq!(row_count(&snapshot), 6000);
        let integrity: String =
            Connection::open_with_flags(&snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap()
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .unwrap();
        assert_eq!(integrity, "ok");
        // The copy is page-for-page, so it is as large as the source's pages.
        assert_eq!(
            std::fs::metadata(&snapshot).unwrap().len(),
            report.page_count * 4096
        );
        let names = file_names(temp.path());
        assert!(names.contains(&"snapshot.db".to_string()), "{names:?}");
        assert!(
            names.iter().all(|name| !name.starts_with("snapshot.db-")),
            "snapshot left sidecars: {names:?}"
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&snapshot), 0o600);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_high_freelist_uses_vacuum_into_and_drops_free_pages() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let conn = rows_database(&source, 600);
        conn.execute("DELETE FROM rows WHERE id > 200", []).unwrap();
        let snapshot = temp.path().join("snapshot.db");
        let report = write_snapshot(&conn, &snapshot).unwrap();
        assert_eq!(report.method, SnapshotMethod::VacuumInto);
        assert!(report.freelist_count * 20 >= report.page_count);
        assert_eq!(version(&snapshot).unwrap(), 10);
        assert_eq!(row_count(&snapshot), 200);
        let kept =
            Connection::open_with_flags(&snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(pragma_count(&kept, "freelist_count").unwrap(), 0);
        assert!(pragma_count(&kept, "page_count").unwrap() < report.page_count);
        #[cfg(unix)]
        assert_eq!(mode_of(&snapshot), 0o600);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_snapshot_never_overwrites_and_failure_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let conn = rows_database(&source, 50);
        // An existing file is refused and left untouched.
        let existing = temp.path().join("existing.db");
        std::fs::write(&existing, b"earlier snapshot").unwrap();
        assert!(write_snapshot(&conn, &existing).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"earlier snapshot");
        // A missing directory fails and creates nothing.
        let missing = temp.path().join("missing").join("snapshot.db");
        assert!(write_snapshot(&conn, &missing).is_err());
        assert!(!temp.path().join("missing").exists());
        // Cleanup after a failed copy removes the file and every sidecar.
        let partial = temp.path().join("partial.db");
        for ending in ["", "-journal", "-wal", "-shm"] {
            std::fs::write(suffix(&partial, ending), b"partial").unwrap();
        }
        remove_partial_snapshot(&partial);
        assert_eq!(
            file_names(temp.path()),
            vec!["existing.db", "source.db", "source.db-shm", "source.db-wal"]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn migration_backup_marker_and_prior_snapshot_removal_hold_for_both_methods() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("rsi.db");
        let conn = rows_database(&database, 600);
        before_migration(&conn, &database).unwrap();
        let first = read_marker(&database).unwrap().unwrap();
        assert_eq!(first.from_version, 10);
        assert_eq!(first.to_version, LATEST_SCHEMA_VERSION);
        assert!(first.failed_path.is_none());
        assert_eq!(row_count(&first.path), 600);
        // A later snapshot over a freelist-heavy database replaces the prior
        // automatic snapshot, by the other method.
        conn.pragma_update(None, "user_version", 9).unwrap();
        conn.execute("DELETE FROM rows WHERE id > 100", []).unwrap();
        before_migration(&conn, &database).unwrap();
        let second = read_marker(&database).unwrap().unwrap();
        assert_ne!(second.path, first.path);
        assert!(!first.path.exists());
        assert_eq!(second.from_version, 9);
        assert_eq!(row_count(&second.path), 100);
        assert_eq!(
            file_names(&temp.path().join("backups")),
            vec![
                second
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
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
