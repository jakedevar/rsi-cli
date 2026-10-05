//! Launch phase breadcrumbs for the watchdog (#1166).
//!
//! A wedged async runtime shows no thread stacks (idle workers and blocked ones
//! look alike, and `ptrace`/`/proc/<tid>/syscall` are unreadable on the hub).
//! Each running launch (and the manager-action loop) records the name of the
//! phase it just entered; the watchdog thread reads the table with `try_lock` at
//! a trip, so the restart evidence names the await each launch was parked at.
//! The table lock is held only for a push/replace/remove, never across an await.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

static PHASES: Mutex<Vec<(Uuid, &'static str, Instant)>> = Mutex::new(Vec::new());

/// Upper bound on tracked entries; a leaked entry cannot grow the table.
const MAX_ENTRIES: usize = 256;

/// Record that `id` entered `phase`.
pub fn note_phase(id: Uuid, phase: &'static str) {
    let mut table = PHASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if let Some(entry) = table.iter_mut().find(|entry| entry.0 == id) {
        entry.1 = phase;
        entry.2 = now;
        return;
    }
    if table.len() < MAX_ENTRIES {
        table.push((id, phase, now));
    }
}

/// Forget `id` (its launch finished or was dropped).
pub fn clear_phase(id: Uuid) {
    PHASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|entry| entry.0 != id);
}

/// Clears the breadcrumb when the launch future ends, however it ends.
pub struct PhaseGuard(Uuid);

impl PhaseGuard {
    #[must_use]
    pub fn new(id: Uuid, first_phase: &'static str) -> Self {
        note_phase(id, first_phase);
        Self(id)
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        clear_phase(self.0);
    }
}

/// `id phase age_secs` lines for every tracked entry. Never blocks: a table
/// locked at that instant reports so instead of waiting.
#[must_use]
pub fn snapshot() -> Vec<String> {
    let table = match PHASES.try_lock() {
        Ok(table) => table,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return vec!["<table busy>".into()],
    };
    let now = Instant::now();
    table
        .iter()
        .map(|(id, phase, since)| {
            format!(
                "{id} {phase} {}s",
                now.saturating_duration_since(*since).as_secs()
            )
        })
        .collect()
}

/// How long ago the oldest tracked phase began, for tests and the trip log.
#[must_use]
pub fn oldest_age() -> Option<Duration> {
    let table = PHASES.try_lock().ok()?;
    let now = Instant::now();
    table
        .iter()
        .map(|entry| now.saturating_duration_since(entry.2))
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_phase_names_the_await_and_clears_with_its_guard() {
        let id = Uuid::new_v4();
        {
            let _guard = PhaseGuard::new(id, "catalog_refresh");
            note_phase(id, "model_admission");
            let lines = snapshot();
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with(&id.to_string())
                        && line.contains("model_admission")),
                "{lines:?}"
            );
        }
        assert!(
            snapshot()
                .iter()
                .all(|line| !line.contains(&id.to_string()))
        );
    }
}
