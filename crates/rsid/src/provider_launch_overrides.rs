//! Operator provider launch overrides at launch time (hub #2765, #2748 G14).
//!
//! The validated list lives in `RuntimeConfig::provider_launch_overrides`
//! (vocabulary and checks: `rsi_common::provider_launch_overrides`). The Claude
//! and Codex CLI clients append it to each launch's argv and note here what
//! they applied for the session; the session monitor takes that note on the
//! run's first event and records it on the session as a `System` row
//! (`metadata.provider_health.kind = "launch_overrides"`,
//! `provider_stream_health::launch_overrides_event`). An empty list applies
//! nothing and records nothing.

use rsi_common::provider_launch_overrides::{OverrideProvider, ProviderLaunchOverrides};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

/// Notes waiting for their monitor. A note nobody takes (a launch whose
/// monitor never saw an event) is dropped once this many are waiting.
const MAX_PENDING: usize = 1024;

/// What one launch applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedLaunchOverrides {
    pub provider: OverrideProvider,
    /// The canonical `key=value` entries.
    pub entries: Vec<String>,
    /// The exact arguments appended to the CLI argv.
    pub args: Vec<String>,
}

fn pending() -> &'static Mutex<HashMap<Uuid, AppliedLaunchOverrides>> {
    static PENDING: OnceLock<Mutex<HashMap<Uuid, AppliedLaunchOverrides>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The arguments a launch of `provider` appends, and the note to record. `None`
/// when the operator configured nothing for `provider`.
#[must_use]
pub fn resolve(
    overrides: &ProviderLaunchOverrides,
    provider: OverrideProvider,
) -> Option<AppliedLaunchOverrides> {
    let entries = overrides.entry_texts(provider);
    if entries.is_empty() {
        return None;
    }
    let args = match provider {
        OverrideProvider::Claude => overrides.claude_args(),
        OverrideProvider::Codex => overrides.codex_args(),
    };
    Some(AppliedLaunchOverrides {
        provider,
        entries,
        args,
    })
}

/// Record what a launch of `session_id` applied; the latest launch wins.
pub fn note_applied(session_id: Uuid, applied: AppliedLaunchOverrides) {
    let mut pending = pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending.len() >= MAX_PENDING && !pending.contains_key(&session_id) {
        pending.clear();
    }
    pending.insert(session_id, applied);
}

/// Take what the latest launch of `session_id` applied, if anything.
#[must_use]
pub fn take_applied(session_id: Uuid) -> Option<AppliedLaunchOverrides> {
    pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&session_id)
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    use super::*;
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    use rsi_common::provider_launch_overrides::normalize;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn an_empty_list_resolves_to_nothing_and_a_note_is_taken_once() {
        let empty = ProviderLaunchOverrides::default();
        assert_eq!(resolve(&empty, OverrideProvider::Claude), None);
        assert_eq!(resolve(&empty, OverrideProvider::Codex), None);

        let overrides = normalize(&serde_json::json!({"codex": ["model_verbosity=low"]})).unwrap();
        assert_eq!(resolve(&overrides, OverrideProvider::Claude), None);
        let applied = resolve(&overrides, OverrideProvider::Codex).unwrap();
        assert_eq!(applied.entries, vec!["model_verbosity=low"]);
        assert_eq!(applied.args, vec!["-c", "model_verbosity=\"low\""]);

        let session = Uuid::new_v4();
        assert_eq!(take_applied(session), None);
        note_applied(session, applied.clone());
        assert_eq!(take_applied(session), Some(applied));
        assert_eq!(take_applied(session), None);
    }
}
