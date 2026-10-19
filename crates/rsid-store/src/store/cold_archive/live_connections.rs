//! A test seam: which cold-archive pass connections to the live database are
//! open right now (#2384 review, #205).
//!
//! A restart's compaction helper may start only once the archive pass has closed
//! the SQLite handles it opened. A count of open file descriptors cannot show
//! that: SQLite postpones closing a descriptor while another connection of the
//! process holds a lock on the same file, and hands that descriptor to the next
//! connection it opens, so the count neither rises when a pass opens its
//! connection nor falls when it closes it. Each connection
//! [`super::open_read_connection`] returns therefore carries a probe function that
//! owns a sentinel: SQLite destroys the function when it closes the connection,
//! and the sentinel's drop takes the connection off the count. Compiled only under
//! `cfg(any(test, feature = "test-seam"))`; no release build has it.

use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

static OPEN: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);
/// Signalled whenever a connection is counted or taken off the count.
static CHANGED: Condvar = Condvar::new();

fn with<R>(f: impl FnOnce(&mut HashMap<PathBuf, usize>) -> R) -> R {
    let mut guard = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(HashMap::new))
}

/// Pass connections to the database at `path` (as the pass was given it) that
/// are open now: opened by [`super::open_read_connection`] and not yet closed.
#[must_use]
pub fn open_on(path: &Path) -> usize {
    with(|open| open.get(path).copied().unwrap_or(0))
}

/// Waits until exactly `count` pass connections to `path` are open, or `bound`
/// passes, and returns how many were open when it stopped. The wake-up is the
/// change of the count itself (a test that holds a pass on a seam waits for the
/// pass to open its connection); `bound` is only a safety net.
#[must_use]
pub fn wait_for(path: &Path, count: usize, bound: Duration) -> usize {
    let guard = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    let (guard, _timed_out) = CHANGED
        .wait_timeout_while(guard, bound, |open| {
            open.as_ref()
                .and_then(|open| open.get(path))
                .copied()
                .unwrap_or(0)
                != count
        })
        .unwrap_or_else(PoisonError::into_inner);
    guard
        .as_ref()
        .and_then(|open| open.get(path))
        .copied()
        .unwrap_or(0)
}

/// Takes a connection off the count when SQLite destroys its probe function.
struct Sentinel(PathBuf);

impl Drop for Sentinel {
    fn drop(&mut self) {
        with(|open| {
            if let Some(count) = open.get_mut(&self.0) {
                *count = count.saturating_sub(1);
            }
        });
        CHANGED.notify_all();
    }
}

/// Counts `conn` (a connection to the database at `path`) until it is closed.
pub(super) fn track(conn: &Connection, path: &Path) -> rusqlite::Result<()> {
    with(|open| *open.entry(path.to_path_buf()).or_insert(0) += 1);
    CHANGED.notify_all();
    let sentinel = Sentinel(path.to_path_buf());
    conn.create_scalar_function(
        "rsi_cold_connection_probe",
        0,
        FunctionFlags::SQLITE_UTF8,
        move |_ctx| {
            let _owned = &sentinel;
            Ok(0_i64)
        },
    )
}
