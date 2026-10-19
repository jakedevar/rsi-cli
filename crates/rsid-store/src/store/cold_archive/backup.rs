//! The fresh-backup gate of a `move` pass (#2384, GM condition 2).
//!
//! Before **every** deleting transaction (each batch and each isolated retry,
//! not only the pass's first) a move pass requires a readable `SQLite` backup of
//! the live database in `<data_dir>/backups/` that is still younger than
//! [`BACKUP_MAX_AGE`] when the transaction is admitted, carries the live
//! database's `application_id` and a `user_version` no newer than the live one.
//! A pre-migration backup (`rsi-pre-v*.db`) qualifies. The proof is not a flag
//! but a [`FreshBackup`] that is derived again: [`admit_backup`] re-opens the file the
//! last transaction relied on (it may have aged out, been rotated, replaced or
//! removed during a long pass) and takes the next qualifying copy otherwise.
//! When none qualifies, the pass takes its own `VACUUM INTO` copy on the
//! archiver's read connection (no store mutex, no temporary database: `VACUUM
//! INTO` writes straight into the target file), keeps only the newest pass-owned
//! copy, and removes an older one only after the new one opened read-only and
//! qualified. If no backup qualifies after that, the pass refuses
//! [`BACKUP_STALE_CODE`] and deletes nothing more; sessions already moved stay
//! moved and accounted.
//!
//! A backup is admitted only while it keeps [`BACKUP_TXN_RESERVE`] of its life.
//! The proof is checked **again at the transaction boundary**
//! ([`FreshBackup::requalify`]): the pass has waited for the store mutex and for
//! SQLite's write lock by then, so that check runs with both held, immediately
//! before the first delete, and opens the file again (still there, still a
//! readable backup of this database, still with the reserve left). A backup that
//! was removed, replaced or aged out during those waits is therefore never relied
//! on; the pass releases the locks, finds or takes another outside them
//! (`VACUUM INTO` never runs under the store lock) and tries again.

use super::mover::NOT_ADMITTED_CODE;
use super::writer::sync_dir;
use super::{ColdError, ColdResult};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How old a backup may be for a move pass to delete rows.
pub const BACKUP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Life a backup must still have when a deleting transaction is admitted, so it
/// cannot age out while the transaction waits for the store.
pub const BACKUP_TXN_RESERVE: Duration = Duration::from_secs(5 * 60);
/// Added to the clock when a freshly written copy is checked, so a coarse file
/// timestamp never lies after the check.
const TIMESTAMP_SLACK: Duration = Duration::from_millis(20);
/// The oldest a backup may be when a deleting transaction is admitted.
pub const BACKUP_ADMISSION_MAX_AGE: Duration =
    Duration::from_secs(BACKUP_MAX_AGE.as_secs() - BACKUP_TXN_RESERVE.as_secs());
/// The refusal of a move pass without a fresh backup.
pub const BACKUP_STALE_CODE: &str = "cold_archive_backup_stale";
/// File-name prefix of the copies a pass takes itself.
pub const PASS_BACKUP_PREFIX: &str = "rsi-cold-pass-";
/// Suffix of a pass-owned copy while `VACUUM INTO` writes it.
const PARTIAL_SUFFIX: &str = ".tmp";

/// The backup a move pass relies on: the file, how old it was when it was
/// checked, and the live identity it was checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshBackup {
    pub path: PathBuf,
    /// Age of the backup when it was checked.
    pub age: Duration,
    /// True when this pass took the copy itself.
    pub produced: bool,
    /// `(application_id, user_version)` of the live database it was checked
    /// against.
    pub live: (i64, i64),
}

impl FreshBackup {
    /// A proof for a backup that qualified against the live identity `live`.
    fn proof(path: PathBuf, age: Duration, produced: bool, live: (i64, i64)) -> Self {
        Self {
            path,
            age,
            produced,
            live,
        }
    }

    /// The admission test again, at the transaction boundary: the file is still
    /// there, still a readable backup of this database, and at `now` still has the
    /// [`BACKUP_TXN_RESERVE`] of its 24 hours left. It runs with the store lock and
    /// SQLite's write lock held, so it never waits: the backup is read with a zero
    /// busy timeout, and a backup another connection holds a lock on is not
    /// admitted ([`NOT_ADMITTED_CODE`]) instead of waited for; the pass releases the
    /// locks and asks again outside them. No copy is taken here.
    ///
    /// # Errors
    /// [`BACKUP_STALE_CODE`] when it no longer qualifies; [`NOT_ADMITTED_CODE`]
    /// when reading it would have had to wait for another connection.
    pub fn requalify(&self, now: SystemTime) -> ColdResult<()> {
        match qualify(
            &self.path,
            self.live,
            now,
            BACKUP_ADMISSION_MAX_AGE,
            Duration::ZERO,
        ) {
            Qualified::Fresh(_) => Ok(()),
            Qualified::Busy => Err(ColdError::new(
                NOT_ADMITTED_CODE,
                format!(
                    "the backup {} is locked by another connection; not waited for with the \
                     store held",
                    self.path.display()
                ),
            )),
            Qualified::Not => Err(ColdError::new(
                BACKUP_STALE_CODE,
                format!(
                    "the backup {} no longer qualifies at the delete transaction (removed, \
                     replaced, unreadable, or out of its reserve)",
                    self.path.display()
                ),
            )),
        }
    }
}

/// `<directory of the database>/backups`, where the pre-migration backups live.
#[must_use]
pub fn backups_dir(database_path: &Path) -> PathBuf {
    database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

/// `(application_id, user_version)` of the database `conn` reads.
fn identity(conn: &Connection) -> ColdResult<(i64, i64)> {
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok((application_id, user_version))
}

/// How long reading a backup outside the store locks waits for another
/// connection's lock on it (`SQLite`'s own default for a new connection).
const QUALIFY_BUSY: Duration = Duration::from_millis(5000);

/// What reading a candidate backup found.
enum Qualified {
    /// A readable backup of this database, this old.
    Fresh(Duration),
    /// Reading it would have had to wait for another connection's lock.
    Busy,
    /// Not a qualifying backup: missing, not a file, too old, unreadable, not a
    /// database, or another database.
    Not,
}

/// `(application_id, user_version, table count)` of an opened backup.
fn read_backup(conn: &Connection) -> rusqlite::Result<(i64, i64, i64)> {
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // Reading the schema proves the file is a database, not just a header.
    let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))?;
    Ok((application_id, user_version, tables))
}

fn is_contention(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Reads `path` as a candidate backup of a database with identity `live` that is
/// at most `max_age` old at `now`, waiting at most `busy` for another connection's
/// lock on it. A modification time in the future does not count as fresh.
fn qualify(
    path: &Path,
    live: (i64, i64),
    now: SystemTime,
    max_age: Duration,
    busy: Duration,
) -> Qualified {
    let Ok(meta) = std::fs::metadata(path) else {
        return Qualified::Not;
    };
    if !meta.is_file() {
        return Qualified::Not;
    }
    let Some(age) = meta
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
    else {
        return Qualified::Not;
    };
    if age > max_age {
        return Qualified::Not;
    }
    let Ok(conn) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Qualified::Not;
    };
    if conn.busy_timeout(busy).is_err() {
        return Qualified::Not;
    }
    let (application_id, user_version, tables) = match read_backup(&conn) {
        Ok(values) => values,
        Err(error) if is_contention(&error) => return Qualified::Busy,
        Err(_) => return Qualified::Not,
    };
    if application_id == live.0 && user_version > 0 && user_version <= live.1 && tables > 0 {
        return Qualified::Fresh(age);
    }
    Qualified::Not
}

/// The age of `path` when it is a readable backup of a database with identity
/// `live` that is at most `max_age` old at `now`; `None` otherwise. A
/// modification time in the future does not count as fresh. Waits up to
/// [`QUALIFY_BUSY`] for another connection's lock: only ever called outside the
/// store locks.
#[must_use]
pub fn qualifying_age(
    path: &Path,
    live: (i64, i64),
    now: SystemTime,
    max_age: Duration,
) -> Option<Duration> {
    match qualify(path, live, now, max_age, QUALIFY_BUSY) {
        Qualified::Fresh(age) => Some(age),
        Qualified::Busy | Qualified::Not => None,
    }
}

fn is_backup_name(name: &str) -> bool {
    name.ends_with(".db")
}

/// The newest qualifying backup in `dir`, if any.
#[must_use]
pub fn find_fresh_backup(
    dir: &Path,
    live: (i64, i64),
    now: SystemTime,
    max_age: Duration,
) -> Option<FreshBackup> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .filter(|entry| is_backup_name(&entry.file_name().to_string_lossy()))
        .filter_map(|entry| {
            let path = entry.path();
            qualifying_age(&path, live, now, max_age)
                .map(|age| FreshBackup::proof(path, age, false, live))
        })
        .min_by_key(|backup| backup.age)
}

/// Creates `path` owner-only; refuses an existing file.
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

fn remove_with_sidecars(path: &Path) {
    for ending in ["", "-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(ending);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

/// Pass-owned files in `dir`: `(path, is_partial)`.
fn pass_owned(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(PASS_BACKUP_PREFIX) {
                return None;
            }
            if name.ends_with(PARTIAL_SUFFIX) {
                Some((entry.path(), true))
            } else if is_backup_name(&name) {
                Some((entry.path(), false))
            } else {
                None
            }
        })
        .collect()
}

/// Takes a pass-owned `VACUUM INTO` copy of the database `read` reads, into
/// `dir`. The copy is written under a partial name, fsynced and renamed, so a
/// crash never leaves a half-written file that looks like a backup. Older
/// pass-owned copies are removed only after the new one qualifies.
fn produce_backup(
    read: &Connection,
    dir: &Path,
    live: (i64, i64),
    now: SystemTime,
) -> ColdResult<FreshBackup> {
    let began = std::time::Instant::now();
    rsid_core::private_state::create_private_dir_all(dir)?;
    for (path, partial) in pass_owned(dir) {
        if partial {
            remove_with_sidecars(&path);
        }
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
    let target = dir.join(format!("{PASS_BACKUP_PREFIX}{stamp}.db"));
    let partial = dir.join(format!("{PASS_BACKUP_PREFIX}{stamp}.db{PARTIAL_SUFFIX}"));
    let partial_text = partial
        .to_str()
        .ok_or_else(|| ColdError::new(BACKUP_STALE_CODE, "backup path is not UTF-8"))?
        .to_string();
    create_new_private(&partial)?;
    let written = read
        .execute("VACUUM INTO ?1", [partial_text.as_str()])
        .map_err(ColdError::from)
        .and_then(|_| {
            std::fs::File::open(&partial)?
                .sync_all()
                .map_err(Into::into)
        })
        .and_then(|()| std::fs::rename(&partial, &target).map_err(Into::into))
        .and_then(|()| sync_dir(dir).map_err(Into::into));
    if let Err(error) = written {
        remove_with_sidecars(&partial);
        return Err(error);
    }
    // The copy was written after `now` was read: qualify it at the instant it was
    // finished (the same clock, moved on by the real time the copy took, plus a
    // little for file systems with coarse timestamps), or its own modification
    // time would lie in the "future" and never count as fresh.
    let taken_at = now + began.elapsed() + TIMESTAMP_SLACK;
    let Some(age) = qualifying_age(&target, live, taken_at, BACKUP_ADMISSION_MAX_AGE) else {
        remove_with_sidecars(&target);
        return Err(ColdError::new(
            BACKUP_STALE_CODE,
            format!("the copy {} did not open as a backup", target.display()),
        ));
    };
    for (path, partial) in pass_owned(dir) {
        if !partial && path != target {
            remove_with_sidecars(&path);
        }
    }
    tracing::info!(backup = %target.display(), "cold archive: took a fresh database backup before moving rows");
    Ok(FreshBackup::proof(target, age, true, live))
}

/// GM condition 2: the backup a move pass relies on, found or taken now.
///
/// # Errors
/// [`BACKUP_STALE_CODE`] when no backup qualifies and none could be taken; the
/// caller must not delete anything then.
pub fn ensure_fresh_backup(
    read: &Connection,
    database_path: &Path,
    now: SystemTime,
) -> ColdResult<FreshBackup> {
    let dir = backups_dir(database_path);
    let live = identity(read)?;
    if let Some(found) = find_fresh_backup(&dir, live, now, BACKUP_ADMISSION_MAX_AGE) {
        return Ok(found);
    }
    produce_backup(read, &dir, live, now).map_err(|error| {
        ColdError::new(
            BACKUP_STALE_CODE,
            format!(
                "no readable backup younger than 24 h in {} and the pass could not take one: {error}",
                dir.display()
            ),
        )
    })
}

/// The backup that admits the next deleting transaction at `now`: `previous`
/// when its file still opens as a qualifying backup of this database and keeps
/// [`BACKUP_TXN_RESERVE`] of its life, else the next one found or taken
/// ([`ensure_fresh_backup`]). A backup that aged out, was rotated, replaced by
/// something that is not a database, or removed since the last transaction is
/// therefore never relied on again (#2384 review: the proof is re-derived per
/// transaction, not carried as a once-true flag).
///
/// # Errors
/// [`BACKUP_STALE_CODE`] when no backup qualifies and none could be taken.
pub fn admit_backup(
    previous: Option<&FreshBackup>,
    read: &Connection,
    database_path: &Path,
    now: SystemTime,
) -> ColdResult<FreshBackup> {
    let Some(previous) = previous else {
        return ensure_fresh_backup(read, database_path, now);
    };
    let live = identity(read)?;
    match qualifying_age(&previous.path, live, now, BACKUP_ADMISSION_MAX_AGE) {
        Some(age) => Ok(FreshBackup::proof(
            previous.path.clone(),
            age,
            previous.produced,
            live,
        )),
        None => ensure_fresh_backup(read, database_path, now),
    }
}
