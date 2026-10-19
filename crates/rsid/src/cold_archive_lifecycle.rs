//! The fence between a cold-archive pass thread and a supervised restart
//! (#2384 review, #205).
//!
//! A pass runs on its own OS thread (`session::cold_archive`) with a read
//! connection of its own, and the thread's join handle is not kept. Shutting the
//! async runtime down cancels the receiver of that thread's result, never the
//! thread, so the compaction helper (which opens the same database after the
//! runtime teardown) could start while a pass still reads, backs up or deletes.
//! This is the lifecycle the pass threads and the restart agree on:
//!
//! 1. **Admission.** [`ArchiveLifecycle::admit`] gives a pass thread a
//!    [`PassTicket`] unless a restart has closed admission. The ticket lives as
//!    long as the thread, after the pass has dropped every SQLite handle it
//!    opened, so the number of tickets is the number of threads that may still
//!    hold one.
//! 2. **Cancel.** [`ArchiveLifecycle::close`] also raises the cancel flag the
//!    pass reads at every safe boundary (between sessions, between move batches,
//!    while the host-load gate is closed, between reclaim steps). The pass
//!    finishes the unit it is in, opens no further transaction and returns; what
//!    it staged and moved stays, the rest waits for the next pass.
//! 3. **Quiescence.** [`ArchiveLifecycle::quiesce`] waits, for a bounded time,
//!    until every ticket is back. Only then does the exiting daemon launch the
//!    compaction helper. If the wait runs out the compaction is deferred (never
//!    an unbounded wait) and the daemon still exits 75.
//!
//! The daemon installs its session manager's instance as the process-wide one
//! ([`install_process`]) at startup; code that has no manager at hand (the
//! deploy loop's quiet-point decision, `main`'s exit path) reaches it through
//! [`process`]. Where none is installed (a unit test) those calls do nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// How long the exiting daemon waits for a cancelled pass to settle before it
/// defers the compaction.
pub const QUIESCE_WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Default)]
struct State {
    closed: bool,
    active: usize,
}

/// Admission, cancel and quiescence of the cold-archive pass threads.
#[derive(Debug, Default)]
pub struct ArchiveLifecycle {
    state: Mutex<State>,
    settled: Condvar,
    cancel: AtomicBool,
}

/// How a bounded wait for the pass threads ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiescence {
    /// No pass thread is alive: none holds a SQLite handle.
    Quiet,
    /// This many pass threads outlived the wait.
    Busy { active: usize },
}

/// A pass thread's right to run. Dropping it (the thread is done and its
/// connections are closed) wakes whoever waits in [`ArchiveLifecycle::quiesce`].
#[derive(Debug)]
pub struct PassTicket {
    lifecycle: Arc<ArchiveLifecycle>,
}

impl Drop for PassTicket {
    fn drop(&mut self) {
        {
            let mut state = self.lifecycle.lock();
            state.active = state.active.saturating_sub(1);
        }
        self.lifecycle.settled.notify_all();
    }
}

impl ArchiveLifecycle {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A ticket for a new pass thread, or `None` while admission is closed.
    #[must_use]
    pub fn admit(self: &Arc<Self>) -> Option<PassTicket> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(PassTicket {
            lifecycle: Arc::clone(self),
        })
    }

    /// A supervised restart is draining: no further pass starts, and a running
    /// pass is asked to stop at its next safe boundary.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.cancel.store(true, Ordering::Release);
    }

    /// The restart did not happen: passes may start again.
    pub fn reopen(&self) {
        let mut state = self.lock();
        state.closed = false;
        self.cancel.store(false, Ordering::Release);
    }

    /// Whether the running pass is asked to stop.
    #[must_use]
    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Whether a new pass would be admitted.
    #[must_use]
    pub fn admitting(&self) -> bool {
        !self.lock().closed
    }

    /// Pass threads alive now.
    #[must_use]
    pub fn active(&self) -> usize {
        self.lock().active
    }

    /// Closes admission, asks any running pass to stop, and waits at most `wait`
    /// for every pass thread to finish.
    #[must_use]
    pub fn quiesce(&self, wait: Duration) -> Quiescence {
        self.close();
        let deadline = Instant::now() + wait;
        let mut state = self.lock();
        loop {
            if state.active == 0 {
                return Quiescence::Quiet;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Quiescence::Busy {
                    active: state.active,
                };
            }
            state = match self.settled.wait_timeout(state, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

static PROCESS: OnceLock<Arc<ArchiveLifecycle>> = OnceLock::new();

/// Makes `lifecycle` the process-wide one (the daemon's session manager does
/// this at startup). The first installation wins.
pub fn install_process(lifecycle: Arc<ArchiveLifecycle>) {
    let _ = PROCESS.set(lifecycle);
}

/// The process-wide lifecycle, once the daemon installed it.
#[must_use]
pub fn process() -> Option<&'static Arc<ArchiveLifecycle>> {
    PROCESS.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn closing_refuses_new_passes_and_reopening_admits_them_again() {
        let lifecycle = Arc::new(ArchiveLifecycle::default());
        let ticket = lifecycle.admit().expect("admitted while open");
        assert_eq!(lifecycle.active(), 1);
        assert!(!lifecycle.cancel_requested());

        lifecycle.close();
        assert!(lifecycle.cancel_requested());
        assert!(!lifecycle.admitting());
        assert!(lifecycle.admit().is_none(), "no pass starts while draining");

        lifecycle.reopen();
        assert!(!lifecycle.cancel_requested());
        assert!(lifecycle.admit().is_some());
        drop(ticket);
        assert_eq!(lifecycle.active(), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn quiescence_waits_for_the_pass_thread_to_return_its_ticket() {
        let lifecycle = Arc::new(ArchiveLifecycle::default());
        let ticket = lifecycle.admit().unwrap();
        let worker = std::thread::spawn({
            let lifecycle = Arc::clone(&lifecycle);
            move || {
                // The pass: runs until it is asked to stop, then lets go.
                while !lifecycle.cancel_requested() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                drop(ticket);
            }
        });
        assert_eq!(
            lifecycle.quiesce(Duration::from_secs(30)),
            Quiescence::Quiet
        );
        assert_eq!(lifecycle.active(), 0);
        worker.join().unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_pass_that_never_returns_makes_the_wait_end_busy_within_its_bound() {
        let lifecycle = Arc::new(ArchiveLifecycle::default());
        let ticket = lifecycle.admit().unwrap();
        let started = Instant::now();
        assert_eq!(
            lifecycle.quiesce(Duration::from_millis(200)),
            Quiescence::Busy { active: 1 }
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        drop(ticket);
        assert_eq!(lifecycle.quiesce(Duration::ZERO), Quiescence::Quiet);
    }
}
