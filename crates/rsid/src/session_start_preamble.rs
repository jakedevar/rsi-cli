//! hub #2919 (local #280, #2748 G15): the RSI preamble a `SessionStart`
//! provider lifecycle hook delivers instead of the first prompt.
//!
//! With `SessionStart` in the operator setting `provider_lifecycle_hooks`, a
//! fresh Claude or Codex launch whose hook is installed (a session identity
//! and the daemon's `rsi-rpc` sibling) and whose preamble fits
//! [`MAX_SESSION_START_CONTEXT_CHARS`] leaves the preamble out of its prompt
//! and [`defer`]s it here. The session's `SessionStart` hook claims it through
//! `RecordProviderLifecycleEvent` ([`claim_for_hook`]) and prints it as
//! `additionalContext`. Every other launch passes exactly the prompt it passed
//! before this module existed.
//!
//! Fallback: a preamble still owed when the session's next launch is built
//! (the hook did not run, or the daemon could not answer it) is put back in
//! that launch's prompt ([`take_undelivered`]); the hook of that launch then
//! finds nothing owed, so the preamble is never doubled.
//!
//! The state is in memory (like `provider_launch_overrides::note_applied`),
//! bounded to [`MAX_TRACKED_SESSIONS`] sessions. A daemon restart forgets it:
//! a hook then delivers nothing and the next fresh launch decides again.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use rsi_common::provider_lifecycle_hooks::MAX_SESSION_START_CONTEXT_CHARS;
use uuid::Uuid;

/// Sessions whose deferred preamble is remembered; the oldest is forgotten.
pub const MAX_TRACKED_SESSIONS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// The launch left the preamble out; the hook has not delivered it.
    Owed,
    /// The hook delivered it.
    Delivered,
}

struct Entry {
    text: String,
    delivery: Delivery,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<Uuid, Entry>,
    order: VecDeque<Uuid>,
}

impl Registry {
    fn insert(&mut self, session_id: Uuid, entry: Entry) {
        if self.entries.insert(session_id, entry).is_none() {
            self.order.push_back(session_id);
        }
        while self.order.len() > MAX_TRACKED_SESSIONS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn remove(&mut self, session_id: Uuid) -> Option<Entry> {
        let entry = self.entries.remove(&session_id)?;
        self.order.retain(|id| *id != session_id);
        Some(entry)
    }
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether `text` can ride a `SessionStart` hook: non-blank and within the
/// provider's `additionalContext` cap.
#[must_use]
pub fn fits(text: &str) -> bool {
    !text.trim().is_empty() && text.chars().count() <= MAX_SESSION_START_CONTEXT_CHARS
}

/// Launch side: this launch left `text` out of its prompt, so the session's
/// `SessionStart` hook owes it. Replaces any earlier entry (a fresh start).
pub fn defer(session_id: Uuid, text: String) {
    registry().insert(
        session_id,
        Entry {
            text,
            delivery: Delivery::Owed,
        },
    );
}

/// Launch side: this launch's prompt carries the preamble itself, so nothing
/// is owed and a later compaction re-delivers nothing.
pub fn forget(session_id: Uuid) {
    registry().remove(session_id);
}

/// Launch side, the fallback: the preamble a launch must put back in its
/// prompt because the hook never delivered it. Taking it ends the debt.
#[must_use]
pub fn take_undelivered(session_id: Uuid) -> Option<String> {
    let mut registry = registry();
    if registry.entries.get(&session_id)?.delivery != Delivery::Owed {
        return None;
    }
    registry.remove(session_id).map(|entry| entry.text)
}

/// Hook side: the preamble the `SessionStart` hook of `session_id` prints for
/// `source`. An owed preamble is delivered once, whatever the source; a
/// delivered one again only after the provider dropped it from the context
/// (`compact`, `clear`). `None` otherwise (a resume, or nothing deferred).
#[must_use]
pub fn claim_for_hook(session_id: Uuid, source: &str) -> Option<String> {
    let mut registry = registry();
    let entry = registry.entries.get_mut(&session_id)?;
    match entry.delivery {
        Delivery::Owed => {
            entry.delivery = Delivery::Delivered;
            Some(entry.text.clone())
        }
        Delivery::Delivered if matches!(source, "compact" | "clear") => Some(entry.text.clone()),
        Delivery::Delivered => None,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_owed_preamble_is_delivered_once_and_again_only_after_compaction() {
        let session = Uuid::new_v4();
        assert_eq!(claim_for_hook(session, "startup"), None);
        defer(session, "PREAMBLE".to_string());
        assert_eq!(
            claim_for_hook(session, "startup").as_deref(),
            Some("PREAMBLE")
        );
        assert_eq!(claim_for_hook(session, "resume"), None);
        assert_eq!(claim_for_hook(session, "startup"), None);
        assert_eq!(
            claim_for_hook(session, "compact").as_deref(),
            Some("PREAMBLE")
        );
        assert_eq!(take_undelivered(session), None, "delivered, not owed");
        forget(session);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_undelivered_preamble_falls_back_once_and_the_hook_then_finds_nothing() {
        let session = Uuid::new_v4();
        defer(session, "PREAMBLE".to_string());
        assert_eq!(take_undelivered(session).as_deref(), Some("PREAMBLE"));
        assert_eq!(take_undelivered(session), None);
        assert_eq!(claim_for_hook(session, "resume"), None);
        assert_eq!(claim_for_hook(session, "compact"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn the_fit_follows_the_provider_cap() {
        assert!(fits("x"));
        assert!(fits(&"é".repeat(MAX_SESSION_START_CONTEXT_CHARS)));
        assert!(!fits(&"x".repeat(MAX_SESSION_START_CONTEXT_CHARS + 1)));
        assert!(!fits(" \n "));
    }
}
