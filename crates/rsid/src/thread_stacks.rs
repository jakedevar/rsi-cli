//! Bounded, signal-free snapshot of every thread's kernel wait state (#1166).
//!
//! The watchdog sees a wedged daemon only as "every thread is in
//! `__futex_wait`". Before it restarts the process, the watchdog thread reads
//! each thread's `comm`, scheduler state, `wchan` and raw `syscall` line from
//! `/proc/self/task/<tid>`: plain file reads with a size cap, a thread cap and
//! one overall deadline. The `syscall` line carries the futex address and
//! arguments, so threads blocked on the same lock share an address and a
//! blocked thread is told apart from an idle parked one.
//!
//! Frames are deliberately NOT captured for other threads. Doing so needs a
//! per-thread signal whose handler allocates and symbolizes, which is not
//! async-signal-safe: a thread holding the allocator lock would wedge the very
//! restart this runs in. No backtrace is taken at all: it would need the global
//! backtrace lock, which a thread stuck unwinding could hold forever.
//!
//! `/proc` reads of a wedged thread can themselves block in the kernel, so the
//! collection never runs on the watchdog thread: [`write_snapshot_bounded`] runs
//! it on a detached thread and gives up after a hard wall-clock deadline.

#[cfg(target_os = "linux")]
mod imp {
    use std::fmt::Write as _;
    use std::io::Read;
    use std::time::{Duration, Instant};

    const MAX_THREADS: usize = 512;
    const MAX_FILE_BYTES: u64 = 512;

    fn read_bounded(path: &std::path::Path) -> String {
        let Ok(file) = std::fs::File::open(path) else {
            return "?".into();
        };
        let mut text = String::new();
        let _ = file.take(MAX_FILE_BYTES).read_to_string(&mut text);
        text.trim().replace('\n', " ")
    }

    /// The scheduler state letter of a `stat` line (the field after `(comm)`).
    fn state_of(stat: &str) -> &str {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .unwrap_or("?")
    }

    /// One text block describing up to `MAX_THREADS` threads, read incrementally
    /// from the task directory, within `deadline` (checked between threads).
    pub fn thread_wait_report(deadline: Duration) -> String {
        let stop = Instant::now() + deadline;
        let mut out = String::from(
            "# daemon watchdog thread wait snapshot (#1166)\n\
             # Frames are NOT captured for other threads: that needs a per-thread signal whose\n\
             # handler allocates, which is not async-signal-safe and could wedge this restart.\n\
             # Per thread: tid comm state wchan syscall(nr args... sp pc). Threads blocked on one\n\
             # lock share a futex address (the first syscall argument). Thread order is the\n\
             # directory order; at most MAX_THREADS threads are read.\n",
        );
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            out.push_str("# /proc/self/task unreadable\n");
            return out;
        };
        let mut listed = 0usize;
        for entry in entries.flatten() {
            if listed >= MAX_THREADS || Instant::now() >= stop {
                let _ = writeln!(out, "# truncated after {listed} threads");
                break;
            }
            let Some(tid) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            listed += 1;
            let base = std::path::Path::new("/proc/self/task").join(&tid);
            let comm = read_bounded(&base.join("comm"));
            let stat = read_bounded(&base.join("stat"));
            let wchan = read_bounded(&base.join("wchan"));
            let syscall = read_bounded(&base.join("syscall"));
            let _ = writeln!(
                out,
                "tid {tid} comm {comm:?} state {} wchan {wchan} syscall {syscall}",
                state_of(&stat)
            );
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::{Arc, Mutex};

        #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
        #[test]
        fn a_thread_blocked_on_a_std_mutex_is_listed_in_a_futex_wait() {
            let lock = Arc::new(Mutex::new(()));
            let held = lock.lock().unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let waiter_lock = Arc::clone(&lock);
            let waiter = std::thread::Builder::new()
                .name("snap-waiter".into())
                .spawn(move || {
                    started_tx.send(()).unwrap();
                    drop(waiter_lock.lock().unwrap());
                })
                .unwrap();
            started_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(300));

            let report = thread_wait_report(Duration::from_secs(2));
            drop(held);
            waiter.join().unwrap();

            assert!(report.contains("NOT captured"), "{report}");
            let line = report
                .lines()
                .find(|line| line.contains("snap-waiter"))
                .unwrap_or_else(|| panic!("the waiter is listed: {report}"));
            assert!(line.contains("futex"), "blocked in a futex wait: {line}");
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::thread_wait_report;

/// Collect a snapshot with `collect` and write it to `path`, all on a detached
/// thread, waiting at most `deadline` of wall clock for it. Returns whether the
/// file was written in time. The caller (the watchdog) proceeds to restart the
/// process regardless: a collector blocked in the kernel is leaked and dies with
/// the process. A failed spawn is logged and reported as `false`.
pub fn write_snapshot_bounded(
    path: std::path::PathBuf,
    deadline: std::time::Duration,
    collect: impl FnOnce() -> String + Send + 'static,
) -> bool {
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<std::io::Result<()>>(1);
    let spawned = std::thread::Builder::new()
        .name("watchdog-stacks".into())
        .spawn(move || {
            let result = std::fs::write(&path, collect());
            let _ = done_tx.send(result);
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "watchdog thread wait snapshot could not start");
        return false;
    }
    match done_rx.recv_timeout(deadline) {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::error!(%error, "watchdog thread wait snapshot could not be persisted");
            false
        }
        Err(_) => {
            tracing::error!("watchdog thread wait snapshot timed out; restarting without it");
            false
        }
    }
}

/// Run `collect` on a detached thread and wait at most `deadline` of wall clock
/// for its value; `None` on timeout or a failed spawn. A collector blocked in the
/// kernel is leaked and dies with the process, so it can never hold up the
/// caller (the watchdog) past the deadline.
pub fn bounded_call<T: Send + 'static>(
    name: &str,
    deadline: std::time::Duration,
    collect: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<T>(1);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = done_tx.send(collect());
        })
        .ok()?;
    done_rx.recv_timeout(deadline).ok()
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_collector_that_never_returns_cannot_delay_the_restart_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stacks.txt");
        let started = Instant::now();
        let written = write_snapshot_bounded(path.clone(), Duration::from_millis(300), || {
            loop {
                std::thread::park();
            }
        });
        let elapsed = started.elapsed();
        assert!(!written);
        assert!(
            elapsed < Duration::from_millis(1500),
            "the restart path waited {elapsed:?} for a stalled collector"
        );
        assert!(!path.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_prompt_collector_is_written_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stacks.txt");
        assert!(write_snapshot_bounded(
            path.clone(),
            Duration::from_secs(2),
            || { "snapshot".to_owned() }
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "snapshot");
    }
}

/// Non-Linux stub: there is no `/proc` to read.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn thread_wait_report(_deadline: std::time::Duration) -> String {
    "# thread wait snapshot is available on Linux only\n".into()
}
