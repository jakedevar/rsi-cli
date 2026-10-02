use super::{ReadError, Result};
use crate::store::Store;
use rusqlite::{Connection, ErrorCode};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::OwnedMutexGuard;

const REMOTE_BUSY_TIMEOUT: Duration = Duration::from_millis(50);
const NORMAL_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const SQL_DEADLINE: Duration = Duration::from_millis(50);
const PROGRESS_GRANULARITY: i32 = 100;
const MAX_VM_STEPS: u32 = 100_000;

/// Run one Remote read under an already acquired Store lock. The caller must
/// acquire that lock in its blocking worker after runtime captures and keep
/// its Remote permit through transport settlement. No transaction crosses the
/// closure. A failed rollback or setting restoration retains the Store lock
/// for process recovery instead of exposing a contaminated connection.
pub fn with_store_budget<T>(
    guard: OwnedMutexGuard<Store>,
    work: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    with_store_budget_deadline(guard, SQL_DEADLINE, work)
}

/// `with_store_budget` with the Store transaction's wall-clock deadline given
/// by the caller (the limiter owns it, so a test can widen or zero it).
pub(super) fn with_store_budget_deadline<T>(
    guard: OwnedMutexGuard<Store>,
    sql_deadline: Duration,
    work: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let conn = &guard.conn;
    if !conn.is_autocommit() {
        std::mem::forget(guard);
        return Err(ReadError::SourceUnavailable);
    }
    let previous_millis: u64 = match conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0)) {
        Ok(value) => value,
        Err(_) => {
            std::mem::forget(guard);
            return Err(ReadError::SourceUnavailable);
        }
    };
    if previous_millis != NORMAL_BUSY_TIMEOUT.as_millis() as u64 {
        std::mem::forget(guard);
        return Err(ReadError::SourceUnavailable);
    }
    if let Err(error) = conn.busy_timeout(REMOTE_BUSY_TIMEOUT) {
        if conn.busy_timeout(NORMAL_BUSY_TIMEOUT).is_err() {
            std::mem::forget(guard);
            return Err(ReadError::SourceUnavailable);
        }
        return Err(ReadError::Sql(error));
    }
    let started = Instant::now();
    let steps = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&steps);
    conn.progress_handler(
        PROGRESS_GRANULARITY,
        Some(move || {
            counter.fetch_add(PROGRESS_GRANULARITY as u32, Ordering::Relaxed)
                + PROGRESS_GRANULARITY as u32
                >= MAX_VM_STEPS
                || started.elapsed() >= sql_deadline
        }),
    );

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let tx = match conn.unchecked_transaction() {
            Ok(tx) => tx,
            Err(error) => return (Err(ReadError::Sql(error)), true),
        };
        let result = work(&tx);
        conn.progress_handler(0, None::<fn() -> bool>);
        let rolled_back = tx.rollback().is_ok();
        (result, rolled_back)
    }));
    conn.progress_handler(0, None::<fn() -> bool>);
    let rollback_ok = if conn.is_autocommit() {
        true
    } else {
        conn.execute_batch("ROLLBACK").is_ok() && conn.is_autocommit()
    };
    let restore = conn.busy_timeout(NORMAL_BUSY_TIMEOUT);
    let inner_rollback_ok = outcome.as_ref().map(|(_, ok)| *ok).unwrap_or(true);
    if !rollback_ok || !inner_rollback_ok || restore.is_err() {
        std::mem::forget(guard);
        if let Err(panic) = outcome {
            resume_unwind(panic);
        }
        return Err(ReadError::SourceUnavailable);
    }
    drop(guard);
    let (result, _) = match outcome {
        Ok(result) => result,
        Err(panic) => resume_unwind(panic),
    };
    if started.elapsed() >= sql_deadline {
        return Err(ReadError::Busy);
    }
    match result {
        Err(ReadError::Sql(rusqlite::Error::SqliteFailure(error, _)))
            if error.code == ErrorCode::OperationInterrupted =>
        {
            Err(ReadError::Busy)
        }
        other => other,
    }
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn remote_store_budget_interrupts_vm_and_restores_shared_connection() {
        let store = Store::open_in_memory().unwrap();
        store.conn.busy_timeout(NORMAL_BUSY_TIMEOUT).unwrap();
        let store = Arc::new(Mutex::new(store));
        let guard = Arc::clone(&store).lock_owned().await;
        let answer = with_store_budget(guard, |conn| {
            conn.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))
                .map_err(ReadError::Sql)
        })
        .unwrap();
        assert_eq!(answer, 7);

        let guard = Arc::clone(&store).lock_owned().await;
        let interrupted = with_store_budget(guard, |conn| {
            conn.query_row(
                "WITH RECURSIVE count(n) AS (
                   SELECT 1 UNION ALL SELECT n+1 FROM count WHERE n<1000000
                 ) SELECT sum(n) FROM count",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(ReadError::Sql)
        });
        assert!(matches!(interrupted, Err(ReadError::Busy)));
        let guard = Arc::clone(&store).lock_owned().await;
        assert!(guard.conn.is_autocommit());
        let timeout: u64 = guard
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 10_000);
        assert_eq!(
            guard
                .conn
                .query_row("SELECT 11", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            11
        );
        drop(guard);

        let guard = Arc::clone(&store).lock_owned().await;
        assert!(matches!(
            with_store_budget(guard, |_conn| -> Result<()> {
                Err(ReadError::InvalidSource)
            }),
            Err(ReadError::InvalidSource)
        ));
        let guard = Arc::clone(&store).lock_owned().await;
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            with_store_budget(guard, |_conn| -> Result<()> {
                panic!("test Remote read worker unwind")
            })
        }));
        assert!(panic.is_err());
        let guard = Arc::clone(&store).lock_owned().await;
        assert!(guard.conn.is_autocommit());
        let timeout: u64 = guard
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 10_000);
    }
}
